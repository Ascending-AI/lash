use super::*;
use lash::TurnId;

#[cfg(test)]
#[path = "tests/support.rs"]
mod support;
use std::future::Future;
pub(crate) use support::*;

#[cfg(test)]
#[path = "tests/mail_payload.rs"]
mod mail_payload_tests;
#[cfg(test)]
#[path = "tests/multi_session.rs"]
mod multi_session_tests;
#[cfg(test)]
#[path = "tests/product_event_persistence.rs"]
mod product_event_persistence_tests;
#[cfg(test)]
#[path = "tests/prompt_sections.rs"]
mod prompt_sections_tests;
#[cfg(test)]
#[path = "tests/recoverable_chat_failures.rs"]
mod recoverable_chat_failures_tests;
#[cfg(test)]
#[path = "tests/recoverable_chat.rs"]
mod recoverable_chat_tests;
#[cfg(test)]
#[cfg(test)]
#[path = "tests/typescript_dialect.rs"]
mod typescript_dialect_tests;

const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

#[test]
fn turn_routing_state_survives_web_process_reconstruction() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_path = temp.path().join("session-id");
    let turns_path = temp.path().join("active-turns.json");
    let sessions = WorkbenchSessions::persistent(session_path.clone()).expect("session ids");
    let session_id = sessions.current();
    let turns = ActiveTurns::persistent(turns_path.clone()).expect("active turns");
    turns.insert_with_prompt(
        &session_id,
        "durable-stop-turn",
        WorkbenchTurnKind::User,
        Some("actual restored prompt".into()),
        None,
    );
    let original_prompt = turns
        .prompt_for(&session_id, &TurnId::from("durable-stop-turn"))
        .expect("claimed UI prompt");
    drop(sessions);
    drop(turns);
    let recovered_ids = WorkbenchSessions::persistent(session_path).expect("recover ids");
    let recovered_turns = ActiveTurns::persistent(turns_path).expect("recover turns");
    assert_eq!(recovered_ids.current(), session_id);
    assert_eq!(
        recovered_turns
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        Some(lash::TurnAddress::new(&session_id, "durable-stop-turn"))
    );
    let recovered_prompt = recovered_turns
        .prompt_for(&session_id, &TurnId::from("durable-stop-turn"))
        .expect("restored prompt");
    assert_eq!(recovered_prompt.text, "actual restored prompt");
    assert_eq!(recovered_prompt.attachment_id, None);
    assert_eq!(recovered_prompt.row_id, original_prompt.row_id);
    assert_eq!(recovered_prompt.at, original_prompt.at);
}

#[cfg(test)]
#[path = "tests/ui_contract.rs"]
mod ui_contract_tests;

#[test]
fn mail_received_account_contract_uses_slugs() {
    const ACCOUNT_SLUG_CONTRACT: &str = "`mail.Received.account` carries the account SLUG, not its display name: use the slug from the account enumeration (for example `work` or `personal`), not a display name such as `Work`, when filtering deliveries.";

    assert!(
        workbench_prompt().contains(ACCOUNT_SLUG_CONTRACT),
        "the workbench prompt must state the mail account slug contract"
    );
}

#[cfg(test)]
#[path = "tests/facade_homes.rs"]
mod facade_homes_tests;

#[test]
fn empty_model_variant_request_clears_selected_variant() {
    let selected_llm_profile = LlmProfileSelection {
        model: "x-ai/grok-build-0.1".to_string(),
        model_variant: Some("medium".to_string()),
    };

    assert_eq!(
        model_variant_for_request(&selected_llm_profile, None),
        Some("medium".to_string())
    );
    assert_eq!(
        model_variant_for_request(&selected_llm_profile, Some(" high ")),
        Some("high".to_string())
    );
    assert_eq!(
        model_variant_for_request(&selected_llm_profile, Some("")),
        None
    );
    assert_eq!(
        model_variant_for_request(&selected_llm_profile, Some("   ")),
        None
    );
}

#[cfg(test)]
#[cfg(test)]
#[path = "tests/concurrent_send.rs"]
mod concurrent_send_tests;
#[cfg(test)]
#[path = "tests/no_progress_budget.rs"]
mod no_progress_budget_tests;
#[cfg(test)]
#[path = "tests/session_fence.rs"]
mod session_fence_tests;
#[cfg(test)]
#[path = "tests/store_maintenance.rs"]
mod store_maintenance_tests;

/// FIG-5045: composer text always uses chat admission and model validation.
#[test]
fn slash_text_uses_chat_admission_and_model_validation() {
    use std::io::Write;
    let node = std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
    let script = r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const html = require('node:fs').readFileSync(0, 'utf8');
const submit = html.split('composer.addEventListener("submit", async event => {')[1].split('\n    });')[0];
const sends = [];
let missingModel = false, validations = 0, focuses = 0;
const context = vm.createContext({
  promptInput: {value: ''}, selectedAttachment: {id: 'image'}, lastUserText: '',
  modelEmpty: () => missingModel,
  validateModel: () => validations++, openModelMenu: () => focuses++,
  selectedModelPayload: () => ({model: 'selected-model'}),
  sendInFlight: false,
  sendTurn: async payload => {sends.push({url: '/api/turn', payload}); return {accepted: true};},
  clearAttachment: () => {context.selectedAttachment = null;}, loadSessions: () => {},
});
vm.runInContext('async function submit(event) {' + submit + '\n}', context);
(async () => {
  for (const text of ['/compact', '/help', 'ordinary chat']) {
    context.promptInput.value = text;
    missingModel = true;
    await context.submit({preventDefault() {}});
    assert.equal(sends.length, validations - 1, 'no admission without a model');
    assert.equal(context.promptInput.value, text, 'validation retains the draft');
    assert.equal(focuses, validations);
    missingModel = false;
    context.selectedAttachment = {id: 'image'};
    await context.submit({preventDefault() {}});
    assert.equal(sends.at(-1).url, '/api/turn');
    assert.equal(sends.at(-1).payload.text, text);
    assert.equal(sends.at(-1).payload.attachment_id, 'image');
    assert.equal(sends.at(-1).payload.model, 'selected-model');
    assert.equal(context.promptInput.value, '');
    assert.equal(context.selectedAttachment, null);
  }
  assert.equal(sends.length, 3);
})().catch(error => {console.error(error); process.exitCode = 1;});
"#;
    let mut child = std::process::Command::new(node)
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run chat admission law");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(ui::INDEX_HTML.as_bytes())
        .expect("send page");
    let output = child.wait_with_output().expect("chat admission law exits");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
