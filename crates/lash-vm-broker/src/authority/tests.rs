use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::effect_opener::EffectOpener;
use lash_vm_protocol::{EffectKind, FrameEpoch, OwnerEpoch, VmOwner};

use super::*;

fn context() -> AdmittedContext {
    AdmittedContext {
        owner: VmOwner::new("turn:a"),
        owner_epoch: OwnerEpoch(0),
        identities: CodeCallIdentities::cell(EffectOpener::turn("session-a", "turn-a"), "cell-1"),
        bindings: Arc::new(FrozenBindings::new().bind(
            "tools",
            "echo",
            BoundOperation {
                tool: ToolRoute {
                    tool_id: "tool:echo".into(),
                    tool_name: "echo".into(),
                },
                arguments: ArgumentContract::Object {
                    required: ["value".to_string()].into(),
                    properties: BTreeSet::new(),
                    additional: false,
                },
            },
        )),
    }
}

fn invoke(binding: &str, operation: &str, arguments: serde_json::Value) -> EncodedPayload {
    Invocation {
        binding: binding.into(),
        operation: operation.into(),
        arguments,
    }
    .request()
    .encode()
}

fn resolve_invoke(payload: &EncodedPayload) -> Result<ResolvedRequest, AuthorityRefusal> {
    resolve(
        &context(),
        &BTreeMap::new(),
        FrameEpoch(0),
        EffectKind::ResourceOperation,
        payload,
    )
}

#[test]
fn a_bound_operation_with_valid_arguments_resolves_to_its_parent_route() {
    let resolved = resolve_invoke(&invoke("tools", "echo", serde_json::json!({"value": 1})))
        .expect("a bound operation resolves");
    let ResolvedRequest::Invoke(call) = resolved else {
        panic!("an invoke resolves to one call: {resolved:?}");
    };
    assert_eq!(call.tool.tool_id, "tool:echo");
}

#[test]
fn every_unauthorised_request_is_refused_typed() {
    let cases = [
        (
            invoke("secrets", "read", serde_json::json!({})),
            "UnknownBinding",
        ),
        (
            invoke("tools", "delete", serde_json::json!({})),
            "UnknownOperation",
        ),
        (
            invoke("tools", "echo", serde_json::json!({"other": 1})),
            "ArgumentsRefused",
        ),
        (
            invoke(
                "tools",
                "echo",
                serde_json::json!({"value": 1, "execution_binding": "x"}),
            ),
            "ArgumentsRefused",
        ),
        (EncodedPayload(b"not a request".to_vec()), "Malformed"),
        (
            OperationRequest::Await(lashlang::Value::String("forged".into())).encode(),
            "KindMismatch",
        ),
    ];
    for (payload, expected) in cases {
        let refusal = resolve_invoke(&payload).expect_err("the request is refused");
        assert!(
            format!("{refusal:?}").starts_with(expected),
            "expected {expected}, got {refusal:?}"
        );
    }
}

#[test]
fn a_handle_is_honoured_only_in_the_frame_that_granted_it() {
    let grant = HandleGrant {
        ordinal: 0,
        call_id: context().identities.call_id(0),
        frame_epoch: FrameEpoch(0),
    };
    let grants = BTreeMap::from([("h-1".to_string(), grant)]);
    let payload = OperationRequest::Await(lashlang::Value::String("h-1".into())).encode();
    assert!(
        resolve(
            &context(),
            &grants,
            FrameEpoch(0),
            EffectKind::Await,
            &payload
        )
        .is_ok()
    );
    assert!(matches!(
        resolve(
            &context(),
            &grants,
            FrameEpoch(1),
            EffectKind::Await,
            &payload
        ),
        Err(AuthorityRefusal::RetiredScope { .. })
    ));
    let forged = OperationRequest::Await(lashlang::Value::String("h-2".into())).encode();
    assert!(matches!(
        resolve(
            &context(),
            &grants,
            FrameEpoch(0),
            EffectKind::Await,
            &forged
        ),
        Err(AuthorityRefusal::UnknownHandle { .. })
    ));
}

#[test]
fn a_fingerprint_is_the_request_content_in_one_spelling() {
    let a = resolve_invoke(&invoke(
        "tools",
        "echo",
        serde_json::json!({"value": {"x": 1, "y": 2}}),
    ))
    .expect("resolves");
    let b = resolve_invoke(&invoke(
        "tools",
        "echo",
        serde_json::json!({"value": {"y": 2, "x": 1}}),
    ))
    .expect("resolves");
    let c = resolve_invoke(&invoke(
        "tools",
        "echo",
        serde_json::json!({"value": {"y": 3, "x": 1}}),
    ))
    .expect("resolves");
    assert_eq!(RequestFingerprint::of(&a), RequestFingerprint::of(&b));
    assert_ne!(RequestFingerprint::of(&a), RequestFingerprint::of(&c));
}
