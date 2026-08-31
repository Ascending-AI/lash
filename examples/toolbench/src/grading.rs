use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::tasks::Task;
use crate::world::World;

pub(crate) const MAX_FAILED_EXECUTIONS: usize = 2;
pub(crate) const IDENTICAL_ERROR_LIMIT: usize = 2;

#[derive(Clone, Debug, Default)]
pub(crate) struct RunEvidence {
    pub(crate) completed: bool,
    pub(crate) completion_error: Option<String>,
    pub(crate) finish_value: Option<Value>,
    pub(crate) iterations: usize,
    pub(crate) tool_call_count: usize,
    pub(crate) failed_execution_errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Grade {
    pub(crate) passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failure_reason: Option<String>,
}

pub(crate) fn grade(task: &Task, final_world: &World, evidence: &RunEvidence) -> Grade {
    let mut failures = Vec::new();

    if !evidence.completed {
        failures.push(format!(
            "turn did not complete{}",
            evidence
                .completion_error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        ));
    }
    if evidence.failed_execution_errors.len() > MAX_FAILED_EXECUTIONS {
        failures.push(format!(
            "{} failed execution iterations exceeds allowance {}",
            evidence.failed_execution_errors.len(),
            MAX_FAILED_EXECUTIONS
        ));
    }
    if let Some((error, count)) = repeated_error(&evidence.failed_execution_errors) {
        failures.push(format!(
            "identical execution error repeated {count} times: {error}"
        ));
    }
    if final_world != &task.expected_world {
        failures.push("mock-world end state differs from the exact expected state".to_string());
    }
    if !task.finish.matches(evidence.finish_value.as_ref()) {
        failures.push(format!(
            "finish mismatch: expected {}; got {}",
            task.finish.describe(),
            evidence
                .finish_value
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "<none>".to_string())
        ));
    }
    if evidence.tool_call_count != task.tool_calls {
        failures.push(format!(
            "tool-call count mismatch: expected {}, got {}",
            task.tool_calls, evidence.tool_call_count
        ));
    }

    Grade {
        passed: failures.is_empty(),
        failure_reason: (!failures.is_empty()).then(|| failures.join("; ")),
    }
}

fn repeated_error(errors: &[String]) -> Option<(&str, usize)> {
    let mut counts = BTreeMap::<&str, usize>::new();
    for error in errors {
        let count = counts.entry(error.as_str()).or_default();
        *count += 1;
        if *count >= IDENTICAL_ERROR_LIMIT {
            return Some((error, *count));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tasks::{FinishMatcher, Task};

    fn fixture() -> Task {
        let seed = World::seeded();
        let mut expected_world = seed.clone();
        expected_world
            .kv
            .insert("status".to_string(), "ready".to_string());
        Task {
            id: "grader-fixture",
            prompt: "fixture",
            seed,
            expected_world,
            finish: FinishMatcher::Exact(json!("saved")),
            tool_calls: 1,
        }
    }

    fn passing_evidence() -> RunEvidence {
        RunEvidence {
            completed: true,
            finish_value: Some(json!("saved")),
            iterations: 1,
            tool_call_count: 1,
            ..RunEvidence::default()
        }
    }

    #[test]
    fn passing_run_is_accepted() {
        let task = fixture();
        assert!(grade(&task, &task.expected_world, &passing_evidence()).passed);
    }

    #[test]
    fn wrong_answer_is_rejected() {
        let task = fixture();
        let mut evidence = passing_evidence();
        evidence.finish_value = Some(json!("not-saved"));
        let result = grade(&task, &task.expected_world, &evidence);
        assert!(!result.passed);
        assert!(result.failure_reason.unwrap().contains("finish mismatch"));
    }

    #[test]
    fn collateral_damage_is_rejected() {
        let task = fixture();
        let mut damaged = task.expected_world.clone();
        damaged.kv.remove("project");
        let result = grade(&task, &damaged, &passing_evidence());
        assert!(!result.passed);
        assert!(result.failure_reason.unwrap().contains("end state"));
    }

    #[test]
    fn wrong_tool_call_count_is_rejected() {
        let task = fixture();
        let mut evidence = passing_evidence();
        evidence.tool_call_count = 2;
        let result = grade(&task, &task.expected_world, &evidence);
        assert!(!result.passed);
        assert!(
            result
                .failure_reason
                .unwrap()
                .contains("tool-call count mismatch: expected 1, got 2")
        );
    }

    #[test]
    fn timed_out_turn_is_rejected() {
        let task = fixture();
        let mut evidence = passing_evidence();
        evidence.completed = false;
        evidence.completion_error =
            Some("turn exceeded the 120 second wall-clock limit".to_string());
        let result = grade(&task, &task.expected_world, &evidence);
        assert!(!result.passed);
        assert_eq!(
            result.failure_reason.as_deref(),
            Some("turn did not complete: turn exceeded the 120 second wall-clock limit")
        );
    }

    #[test]
    fn failed_execution_allowance_is_enforced() {
        let task = fixture();
        let mut evidence = passing_evidence();
        evidence.failed_execution_errors = vec![
            "first execution failed".to_string(),
            "second execution failed".to_string(),
            "third execution failed".to_string(),
        ];
        let result = grade(&task, &task.expected_world, &evidence);
        assert!(!result.passed);
        assert_eq!(
            result.failure_reason.as_deref(),
            Some("3 failed execution iterations exceeds allowance 2")
        );
    }

    #[test]
    fn stuck_identical_error_loop_is_rejected() {
        let task = fixture();
        let mut evidence = passing_evidence();
        evidence.failed_execution_errors = vec![
            "cannot read field owner from string".to_string(),
            "cannot read field owner from string".to_string(),
        ];
        let result = grade(&task, &task.expected_world, &evidence);
        assert!(!result.passed);
        assert!(
            result
                .failure_reason
                .unwrap()
                .contains("identical execution error repeated")
        );
    }
}
