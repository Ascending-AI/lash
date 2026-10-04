//! Refusal classification and preservation through process and host errors.

use super::*;

#[test]
fn l09_run_continuation_refusals_keep_typed_causes_and_live_quiescence() {
    use lash_core::tool_run::ContinuationRefusal;
    for refusal in [
        ContinuationRefusal::NotQuiescent,
        ContinuationRefusal::ForeignOwner,
        ContinuationRefusal::UnleasedMaterial,
        ContinuationRefusal::NotSuccessor { from: 0, found: 2 },
    ] {
        let terminal = refusal != ContinuationRefusal::NotQuiescent;
        let error: lash_core::RuntimeEffectControllerError = refusal.clone().into();
        assert_eq!(error.is_terminal(), terminal);
        assert_eq!(
            error.turn_failure_cause() == lash_core::TurnFailureCause::LiveFault,
            !terminal
        );
        let plugin = lash_core::ProcessInfraError::new(error.clone().into()).into_plugin_error();
        let plugin: PluginError =
            serde_json::from_value(serde_json::to_value(plugin).unwrap()).unwrap();
        let host = plugin.clone().into_turn_failure(RuntimeErrorCode::Plugin);
        assert_eq!(host.cause, error.cause);
        let carried = lash_core::RuntimeEffectControllerError::in_text(&error.to_record()).unwrap();
        assert_eq!(carried.cause, error.cause);
        if !terminal {
            assert!(!is_terminal_runner_error(&plugin, false));
            continue;
        }
        let ProcessAwaitOutput::Settled { output } = terminal_process_output(plugin) else {
            panic!("a permanent refusal settles the process");
        };
        let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("the process retains the refusal");
        };
        let raw = failure.raw.unwrap().to_json_value();
        let carried: lash_core::RuntimeEffectControllerError =
            serde_json::from_value(raw["runtime_error"].clone()).unwrap();
        assert_eq!(carried.cause, error.cause);
    }
}

#[test]
fn session_turn_supersession_keeps_its_cause_across_process_plugin_and_host_boundaries() {
    let runtime = RuntimeError::new(RuntimeErrorCode::StoreCommitSuperseded, "superseded child");
    for plugin in [
        PluginError::Runtime(runtime.clone()),
        PluginError::RuntimeEffectController(runtime.into()),
    ] {
        let plugin = lash_core::ProcessInfraError::new(plugin).into_plugin_error();
        let plugin: PluginError = serde_json::from_value(serde_json::to_value(plugin).unwrap())
            .expect("the plugin boundary retains the typed error");
        assert!(!plugin.is_retryable());
        assert!(!plugin.is_terminal(), "a fresh runtime root can redrive");
        assert!(is_terminal_runner_error(&plugin, true));
        assert!(!is_terminal_runner_error(&plugin, false));
        let runtime = plugin.clone().into_turn_failure(RuntimeErrorCode::Plugin);
        assert_eq!(runtime.code, RuntimeErrorCode::StoreCommitSuperseded);
        assert_eq!(runtime.message, "superseded child");
        let ProcessAwaitOutput::Settled { output } = terminal_process_output(plugin) else {
            panic!("the process settles the refusal");
        };
        let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("the superseded process fails");
        };
        assert_eq!(
            failure.code,
            RuntimeErrorCode::StoreCommitSuperseded.as_str()
        );
        assert_eq!(failure.message, "superseded child");
        assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    }
    for plugin in [
        PluginError::Runtime(RuntimeError::new(
            RuntimeErrorCode::StoreCommitFailed,
            "store I/O",
        )),
        PluginError::attempt_fault("unavailable infrastructure"),
    ] {
        assert!(!is_terminal_runner_error(&plugin, true));
        assert!(!is_terminal_runner_error(&plugin, false));
    }
}
