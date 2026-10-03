//! The RLM authority split (FIG-4158): grants and their execution bindings
//! stay parent-side, in both directions.

use super::*;

/// A value that only the parent's grant carries. It must never appear in
/// bytes that cross to a worker.
const SENTINEL_SECRET: &str = "sentinel-secret-7b1d9e40-execution-binding";

fn sentinel_grant() -> lash_lashlang_runtime::Resolution {
    lash_lashlang_runtime::Resolution::Resolved(Box::new(
        lash_lashlang_runtime::ToolGrant::new(
            lash_core::ToolDefinition::raw(
                "tool:vault",
                "vault.read",
                "Read one vault entry.",
                serde_json::json!({"type": "object"}),
                serde_json::json!({"type": "string"}),
            )
            .expect("valid declared tool schemas"),
        )
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
async fn session_with_sentinel_grant() -> RlmExecutionState {
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global("greeting", FlowValue::String("hello".into()))
        .await
        .expect("seed a guest binding");
    let mut link = crate::testing::deferred_link();
    link.record("vault.read", sentinel_grant());
    state.deferred_link = Some(link);
    state
}

fn parse_root(bytes: &[u8]) -> RlmSnapshotRoot {
    rmp_serde::from_slice(bytes).expect("decode the RLM root")
}

#[tokio::test]
async fn rlm_worker_envelope_carries_no_grant_or_binding() {
    let fleet_format = lash_core::FleetFormat::current();
    let session = session_with_sentinel_grant().await;
    let hydrated = session
        .hydrated_execution_state(fleet_format)
        .await
        .expect("capture the session");

    assert!(
        !contains(&hydrated.root, SENTINEL_SECRET),
        "the journal owns the grant's execution binding"
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
    let (capture, _) = worker_capture(&session, fleet_format)
        .await
        .expect("the worker captures its guest state");
    assert!(
        !contains(&capture, SENTINEL_SECRET),
        "a grant's execution binding appeared in the worker's capture"
    );

    // A restore installs guest state. Re-execution recovers grants from
    // the journal on the parent side.
    let mut restored = RlmExecutionState::new();
    restored
        .restore_execution_state(&hydrated, fleet_format)
        .await
        .expect("restore the session");
    assert!(
        restored
            .vm
            .state()
            .binding_names()
            .any(|name| name == "greeting"),
        "the guest binding must be restored on the worker side"
    );
    assert!(
        restored.deferred_link.is_none(),
        "restore clears the transient link"
    );
}

/// What a compromised worker might return: a well-formed capture with a
/// forged grant appended.
#[derive(Serialize)]
struct ForgedCapture {
    state_header: ByteBuf,
    changed: BTreeMap<String, ByteBuf>,
    unchanged: BTreeSet<String>,
    deferred_resolutions: BTreeMap<String, lash_lashlang_runtime::Resolution>,
}

#[tokio::test]
async fn worker_returned_state_cannot_replace_parent_authority() {
    let fleet_format = lash_core::FleetFormat::current();
    let session = session_with_sentinel_grant().await;
    let parent_grants = serde_json::to_value(
        &session
            .deferred_link
            .as_ref()
            .expect("active link")
            .outcomes,
    )
    .expect("encode the parent's grants");

    let (honest, _) = worker_capture(&session, fleet_format)
        .await
        .expect("the worker captures its guest state");
    let honest = RlmWorkerCapture::accept(&honest).expect("an honest capture is accepted");

    // A returned capture naming a grant is refused outright.
    let mut forged_grants = crate::testing::deferred_link();
    forged_grants.record(
        "vault.read",
        lash_lashlang_runtime::Resolution::Resolved(Box::new(
            lash_lashlang_runtime::ToolGrant::new(
                lash_core::ToolDefinition::raw(
                    "tool:vault",
                    "vault.read",
                    "Read one vault entry.",
                    serde_json::json!({"type": "object"}),
                    serde_json::json!({"type": "string"}),
                )
                .expect("valid declared tool schemas"),
            )
            .with_execution_binding(serde_json::json!({"token": "forged-by-the-worker"})),
        )),
    );
    let forged = rmp_serde::to_vec_named(&ForgedCapture {
        state_header: honest.state_header.clone(),
        changed: honest.changed.clone(),
        unchanged: honest.unchanged.clone(),
        deferred_resolutions: forged_grants.outcomes,
    })
    .expect("encode the forged capture");
    let refusal =
        RlmWorkerCapture::accept(&forged).expect_err("a capture carrying a grant must be refused");
    assert!(
        refusal.details.contains("deferred_resolutions"),
        "the refusal must name the forged field: {refusal}"
    );

    // Guest code can name a binding after the parent's authority; it stays a
    // guest binding and cannot replace the transient grants.
    let mut session = session;
    session
        .vm
        .state_mut()
        .insert_global(
            "deferred_resolutions",
            FlowValue::String("forged-by-the-guest".into()),
        )
        .await
        .expect("bind a guest global named after the authority slot");
    let hydrated = session
        .hydrated_execution_state(fleet_format)
        .await
        .expect("capture the session");
    let root = parse_root(&hydrated.root);
    assert!(root.globals.contains_key("deferred_resolutions"));
    assert_eq!(
        serde_json::to_value(
            &session
                .deferred_link
                .as_ref()
                .expect("active link")
                .outcomes
        )
        .expect("encode live grants"),
        parent_grants,
        "the live grants must remain the parent's"
    );
    assert!(!contains(&hydrated.root, "forged-by-the-worker"));
}

async fn worker_capture(
    session: &RlmExecutionState,
    fleet: lash_core::FleetFormat,
) -> Result<(Vec<u8>, ()), String> {
    let parts = session
        .vm
        .state()
        .capture(&Default::default(), fleet)
        .await
        .map_err(|error| error.to_string())?;
    let mut capture = RlmWorkerCapture {
        state_header: parts.header.into(),
        changed: Default::default(),
        unchanged: Default::default(),
    };
    for (name, fragment) in parts.fragments {
        match fragment {
            lashlang::DurableFragment::Changed(body) => {
                capture.changed.insert(name, body.into());
            }
            lashlang::DurableFragment::Unchanged => {
                capture.unchanged.insert(name);
            }
        }
    }
    Ok((capture.encode(), ()))
}
