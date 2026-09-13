use super::*;
use std::collections::BTreeSet;

// Keep the executable law and its completeness check together. The receiver
// names identify the three sealed surfaces, so a same-named method on another
// capability cannot accidentally satisfy coverage.
pub(super) async fn exercise_attempt_capabilities(attempt: &crate::AttemptContext<'_>) {
    let _ = attempt.session_id();
    let _ = attempt.execution_scope_id();
    let _ = attempt.child_process_parent_scope().await;
    let _ = attempt.agent_frame_id();
    let _ = attempt.cancellation_token();
    let _ = attempt.async_process_id();
    let _ = attempt.runtime_process_id();
    let _ = attempt.attachments();
    let _ = attempt.provider();
    let _ = attempt.prepared_payload();
    let _ = attempt.tool_execution_binding();
    let _ = attempt.tool_call_id();
    let _ = attempt.attempt_number();
    let _ = attempt.max_attempts();
    let _ = attempt.replay_key();
    let _ = attempt.process_execution_env_spec();
    let _ = attempt.decode_prepared_payload::<serde_json::Value>();
    let _ = attempt.named_phase("attempt-capability-law");
    let _ = attempt.completion_key();
    let _ = attempt.intent_identity(0);
    let sessions = attempt.sessions();
    let _ = sessions.model().await;
    let _ = sessions.snapshot_current().await;
    let _ = sessions.snapshot(SESSION).await;
    let _ = sessions.tool_catalog().await;
    let _ = sessions.shared_tool_catalog().await;
    let processes = attempt.processes();
    let _ = processes
        .list_handles_filtered(&crate::ProcessListFilter::default())
        .await;
    assert_eq!(
        attempt
            .direct_completions()
            .complete(
                crate::DirectRequest::text(DIRECT_MODEL, "attempt direct completion"),
                "attempt-atomicity",
            )
            .await
            .expect("attempt-context direct completion stays local")
            .text,
        DIRECT_TEXT,
    );
}

pub(super) fn assert_capability_inventory_complete() {
    let surface = include_str!("../../../tool_provider.rs");
    let law = include_str!("capability_inventory.rs")
        .split_once("async fn exercise_attempt_capabilities(")
        .unwrap()
        .1
        .split("\n}\n")
        .next()
        .unwrap();
    let law: String = law.chars().filter(|c| !c.is_whitespace()).collect();
    for (receiver, type_name) in [
        ("attempt", "AttemptContext"),
        ("sessions", "AttemptSessionReads"),
        ("processes", "AttemptProcessReads"),
    ] {
        // Rustfmt closes top-level impls at column zero. Scan every inherent
        // impl, including later extension blocks, rather than one copied list.
        let mut in_surface = false;
        let mut declared = BTreeSet::new();
        for line in surface.lines() {
            if line.starts_with("impl") {
                in_surface = !line.contains(" for ")
                    && (line.contains(&format!(" {type_name} {{"))
                        || line.contains(&format!(" {type_name}<")));
            } else if line == "}" {
                in_surface = false;
            } else if in_surface {
                let line = line.trim();
                if let Some(signature) = line.strip_prefix("pub ")
                    && let Some((_, name)) = signature.split_once("fn ")
                {
                    let name = name.split(['(', '<']).next().unwrap();
                    // The explicit testing constructor is not a capability
                    // available to a provider body.
                    if name != "__for_testing" {
                        declared.insert(name);
                    }
                }
            }
        }
        assert!(
            !declared.is_empty(),
            "surface parser must discover {type_name}"
        );
        let prefix = format!("{receiver}.");
        let exercised: BTreeSet<_> = law
            .split(&prefix)
            .skip(1)
            .map(|call| call.split(['(', ':']).next().unwrap())
            .collect();
        assert_eq!(
            declared, exercised,
            "every {receiver} capability must execute in the attempt-crossing law"
        );
    }
}
