use super::*;

/// FIG-5037: compaction is session-scoped chat participation. Host policy is
/// checked before the core submits the durable command.
#[tokio::test]
async fn compact_uses_the_selected_session_and_checks_its_authorization() {
    struct DenyCompact;
    impl WorkbenchAuthorizer for DenyCompact {
        fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError> {
            if matches!(action, WorkbenchAuthorizationAction::CompactContext { .. }) {
                return Err(AppError::forbidden("compaction denied"));
            }
            Ok(())
        }
    }
    let double = crate::tests::test_double_backend(5037).await;
    let mut state = recoverable_chat_test_state(&double, 16).await;
    let selected = state.current_session_id();
    let result = compact_context(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(selected.clone()),
        }),
    )
    .await
    .expect("empty context compaction settles through the engine");
    assert_eq!(result.0, json!({"opened": false}));
    state.authorization = WorkbenchAuthorization::with_authorizer(Arc::new(DenyCompact));
    let error = compact_context(
        State(state),
        Query(SessionQuery {
            session_id: Some(selected),
        }),
    )
    .await
    .expect_err("host can deny compaction");
    assert_eq!(error.into_response().status(), StatusCode::FORBIDDEN);
}

/// FIG-5037: only an exact /compact is a composer command, and its one note
/// belongs to the session that submitted it, even across a session switch.
#[test]
fn compact_composer_is_exact_and_fences_the_result_note() {
    use std::io::Write;
    let node = std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
    let script = r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const html = require('node:fs').readFileSync(0, 'utf8');
const source = html.split('// BEGIN WORKBENCH_COMPACT_COMMAND')[1].split('// END WORKBENCH_COMPACT_COMMAND')[0];
const submit = html.split('composer.addEventListener("submit", async event => {')[1].split('\n    });')[0];
let result = {opened: true};
let request;
const notes = [], sends = [], requests = [];
const context = vm.createContext({
  scopedSessionId: 'selected', promptInput: {value: '/compact'},
  fetch: (url, opts) => {requests.push({url, opts}); return new Promise(resolve => request = resolve);},
  renderNote: text => notes.push(text),
  modelEmpty: () => false, selectedAttachment: null, selectedModelPayload: () => ({}),
  postCommand: async (url, payload) => {sends.push({url, payload}); return null;},
  lastUserText: '', loadSessions: () => {}, syncCommandAvailability: () => {},
});
vm.runInContext(source + '\nasync function submit(event) {' + submit + '\n}', context);
const event = {preventDefault() {}};
const answer = () => request({ok: true, json: async () => result});
(async () => {
  const first = context.submit(event);
  const duplicate = context.compactContext();
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, '/api/compact');
  assert.equal(requests[0].opts.method, 'POST');
  answer(); await first; await duplicate;
  assert.deepEqual(notes, ['Context compacted.']);
  assert.equal(sends.length, 0);
  result = {opened: false};
  const empty = context.compactContext(); answer(); await empty;
  assert.equal(notes.at(-1), 'No context to compact yet.');
  result = {pending: {session_id: 'selected'}};
  const pending = context.compactContext(); answer(); await pending;
  assert.match(notes.at(-1), /queued/);
  const switched = context.compactContext();
  context.scopedSessionId = 'another'; answer(); await switched;
  assert.equal(notes.length, 3, 'old session result cannot appear in the new chat');
  context.promptInput.value = '/compact later';
  await context.submit(event);
  assert.equal(sends.length, 1, 'other slash text is ordinary chat input');
  assert.equal(sends[0].payload.text, '/compact later');
})().catch(error => {console.error(error); process.exitCode = 1;});
"#;
    let mut child = std::process::Command::new(node)
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run composer law");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(ui::INDEX_HTML.as_bytes())
        .expect("send page");
    let output = child.wait_with_output().expect("composer law completes");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
