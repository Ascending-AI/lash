//! The RLM authority split (FIG-4158): grants and their execution bindings
//! stay parent-side, in both directions.

use super::*;

/// A value that only the parent's grant carries. It must never appear in
/// bytes that cross to a worker.
const SENTINEL_SECRET: &str = "sentinel-secret-7b1d9e40-execution-binding";

fn sentinel_grant() -> lash_lashlang_runtime::Resolution {
    lash_lashlang_runtime::Resolution::Resolved(Box::new(
        lash_lashlang_runtime::ToolGrant::new(lash_core::ToolDefinition::raw(
            "tool:vault",
            "vault.read",
            "Read one vault entry.",
            serde_json::json!({"type": "object"}),
            serde_json::json!({"type": "string"}),
        ))
        .with_source_id("registry:vault")
        .with_execution_binding(serde_json::json!({"token": SENTINEL_SECRET})),
    ))
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// A session with one guest binding and a recorded grant whose execution
/// binding carries the sentinel.
fn session_with_sentinel_grant() -> RlmExecutionState {
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global("greeting", FlowValue::String("hello".into()))
        .expect("seed a guest binding");
    state
        .deferred_resolutions
        .record("vault.read", sentinel_grant());
    state
}

fn parse_root(bytes: &[u8]) -> RlmSnapshotRoot {
    rmp_serde::from_slice(bytes).expect("decode the RLM root")
}

#[test]
fn rlm_worker_envelope_carries_no_grant_or_binding() {
    let fleet_format = lash_core::FleetFormat::current();
    let session = session_with_sentinel_grant();
    let hydrated = session
        .hydrated_execution_state(fleet_format)
        .expect("capture the session");

    // The parent's durable root keeps the grant, binding and all.
    assert!(
        contains(&hydrated.root, SENTINEL_SECRET),
        "the parent-side root must carry the grant's execution binding"
    );

    // The worker-bound envelope carries the guest state and nothing else.
    let root = parse_root(&hydrated.root);
    let envelope = worker_bound_envelope(&hydrated, &root)
        .expect("assemble the worker-bound envelope")
        .encode();
    assert!(
        !contains(&envelope, SENTINEL_SECRET),
        "a grant's execution binding crossed to the worker"
    );
    for authority in [
        "deferred_resolutions",
        "deferred_trigger_resolutions",
        "vault.read",
    ] {
        assert!(
            !contains(&envelope, authority),
            "the worker-bound envelope names `{authority}`"
        );
    }
    let decoded: RlmWorkerEnvelope =
        rmp_serde::from_slice(&envelope).expect("the envelope decodes as guest state");
    assert_eq!(
        decoded.globals.keys().collect::<Vec<_>>(),
        vec!["greeting"],
        "the envelope carries exactly the guest bindings"
    );

    // What the worker returns carries no authority either.
    let (capture, _) = worker_side::capture(&session.vm, &DurableBaseline::default(), fleet_format)
        .expect("the worker captures its guest state");
    assert!(
        !contains(&capture, SENTINEL_SECRET),
        "a grant's execution binding appeared in the worker's capture"
    );

    // A restore installs the guest state from the envelope and keeps the
    // grant parent-side.
    let mut restored = RlmExecutionState::new();
    restored
        .restore_execution_state(&hydrated, fleet_format)
        .expect("restore the session");
    assert!(
        restored
            .vm
            .state()
            .binding_names()
            .any(|name| name == "greeting"),
        "the guest binding must be restored on the worker side"
    );
    assert_eq!(
        serde_json::to_value(&restored.deferred_resolutions).expect("encode restored grants"),
        serde_json::to_value(&session.deferred_resolutions).expect("encode original grants"),
        "the parent must keep its grants across a restore"
    );
}

/// What a compromised worker might return: a well-formed capture with a
/// forged grant appended.
#[derive(Serialize)]
struct ForgedCapture {
    state_header: ByteBuf,
    changed: BTreeMap<String, ByteBuf>,
    unchanged: BTreeSet<String>,
    deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
}

#[test]
fn worker_returned_state_cannot_replace_parent_authority() {
    let fleet_format = lash_core::FleetFormat::current();
    let session = session_with_sentinel_grant();
    let parent_grants =
        serde_json::to_value(&session.deferred_resolutions).expect("encode the parent's grants");

    let (honest, _) = worker_side::capture(&session.vm, &DurableBaseline::default(), fleet_format)
        .expect("the worker captures its guest state");
    let honest = RlmWorkerCapture::accept(&honest).expect("an honest capture is accepted");

    // A returned capture naming a grant is refused outright.
    let mut forged_grants = lash_lashlang_runtime::DeferredResolutionRecord::default();
    forged_grants.record(
        "vault.read",
        lash_lashlang_runtime::Resolution::Resolved(Box::new(
            lash_lashlang_runtime::ToolGrant::new(lash_core::ToolDefinition::raw(
                "tool:vault",
                "vault.read",
                "Read one vault entry.",
                serde_json::json!({"type": "object"}),
                serde_json::json!({"type": "string"}),
            ))
            .with_execution_binding(serde_json::json!({"token": "forged-by-the-worker"})),
        )),
    );
    let forged = rmp_serde::to_vec_named(&ForgedCapture {
        state_header: honest.state_header.clone(),
        changed: honest.changed.clone(),
        unchanged: honest.unchanged.clone(),
        deferred_resolutions: forged_grants,
    })
    .expect("encode the forged capture");
    let refusal =
        RlmWorkerCapture::accept(&forged).expect_err("a capture carrying a grant must be refused");
    assert!(
        refusal.details.contains("deferred_resolutions"),
        "the refusal must name the forged field: {refusal}"
    );

    // Guest code can name a binding after the parent's authority; it stays a
    // guest binding, and the root's grants still come from the parent alone.
    let mut session = session;
    session
        .vm
        .state_mut()
        .insert_global(
            "deferred_resolutions",
            FlowValue::String("forged-by-the-guest".into()),
        )
        .expect("bind a guest global named after the authority slot");
    let hydrated = session
        .hydrated_execution_state(fleet_format)
        .expect("capture the session");
    let root = parse_root(&hydrated.root);
    assert!(root.globals.contains_key("deferred_resolutions"));
    assert_eq!(
        serde_json::to_value(&root.deferred_resolutions).expect("encode the root's grants"),
        parent_grants,
        "the root's grants must be the parent's, not the worker's"
    );
    assert!(!contains(&hydrated.root, "forged-by-the-worker"));
}
