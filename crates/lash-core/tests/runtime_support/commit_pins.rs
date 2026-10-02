//! Commit-bytes pins shared by the runtime suites: a digest of every runtime
//! commit a turn applied, with the values that differ between two runs of the
//! same turn masked, so a suite can assert a turn commits exactly the bytes it
//! committed before a change (FIG-3672 P6).

/// Object keys whose values are worker, lease or wall-clock facts, or hashes
/// over them: they differ between two runs of the same turn.
const RUN_VARIANT_KEYS: &[&str] = &["incarnation_id", "executor_id", "lease_token"];

fn is_uuid(candidate: &[u8]) -> bool {
    candidate.len() == 36
        && candidate
            .iter()
            .enumerate()
            .all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
}

fn mask_uuids(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut masked = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 36 <= bytes.len() && is_uuid(&bytes[index..index + 36]) {
            masked.push_str("<uuid>");
            index += 36;
        } else {
            let next = text[index..].chars().next().expect("a char at a boundary");
            masked.push(next);
            index += next.len_utf8();
        }
    }
    masked
}

fn is_timestamp(text: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(text).is_ok()
}

fn mask_run_variant(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, field) in map.iter_mut() {
                if RUN_VARIANT_KEYS.contains(&key.as_str()) || key.ends_with("_hash") {
                    *field = serde_json::Value::String("<run-variant>".to_string());
                } else {
                    mask_run_variant(field);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(mask_run_variant),
        serde_json::Value::String(text) => {
            *text = if is_timestamp(text) {
                "<timestamp>".to_string()
            } else {
                mask_uuids(text)
            };
        }
        _ => {}
    }
}

fn value_digest(value: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(value).expect("a masked commit serializes");
    lash_core::stable_hash::sha256_hex(&bytes)
}

pub(crate) fn commit_digest(commit: &lash_core::RuntimeCommit) -> String {
    let mut value = serde_json::to_value(commit).expect("a runtime commit serializes");
    mask_run_variant(&mut value);
    value_digest(&value)
}

struct ShapeChange {
    name: &'static str,
    restore: fn(&mut serde_json::Value),
}

// Each reversal touches only the named fixture delta. Add a reversal when an
// in-place serialized shape changes; the whole old digest remains the oracle.
const SHAPE_CHANGES: &[ShapeChange] = &[
    ShapeChange {
        name: "FIG-2002: remove SessionPolicy.session_id",
        restore: restore_policy_session_id,
    },
    ShapeChange {
        name: "FIG-4655: group output-token limits",
        restore: restore_output_token_limits,
    },
];

fn frame_policies(value: &mut serde_json::Value) -> impl Iterator<Item = &mut serde_json::Value> {
    value
        .pointer_mut("/graph/Extend/nodes")
        .and_then(serde_json::Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter(|node| node["kind"] == "frame_open")
        .filter_map(|node| node.pointer_mut("/assignment/policy"))
}

fn restore_policy_session_id(value: &mut serde_json::Value) {
    for policy in frame_policies(value) {
        policy
            .as_object_mut()
            .expect("a frame policy is an object")
            .entry("session_id")
            .or_insert(serde_json::Value::Null);
    }
}

fn restore_output_token_limits(value: &mut serde_json::Value) {
    fn restore_model(model: &mut serde_json::Value) {
        if let Some(limits) = model
            .pointer_mut("/model/metadata/limits")
            .and_then(serde_json::Value::as_object_mut)
            && limits.get("output_tokens")
                == Some(&serde_json::json!({"capacity": null, "default_cap": null}))
        {
            // These pins record no output cap. Do not erase non-default facts.
            limits.remove("output_tokens");
        }
    }
    if let Some(model) = value.pointer_mut("/config/model") {
        restore_model(model);
    }
    for policy in frame_policies(value) {
        if let Some(model) = policy.get_mut("model") {
            restore_model(model);
        }
    }
}

#[derive(serde::Serialize)]
struct PinProof {
    restored_digest: String,
    shape_changes: Vec<&'static str>,
}

fn prove_pin_delta(
    value: &serde_json::Value,
    expected: &str,
    changes: &[ShapeChange],
) -> Option<PinProof> {
    let digest = value_digest(value);
    if digest == expected {
        return Some(PinProof {
            restored_digest: digest,
            shape_changes: Vec::new(),
        });
    }
    for (index, change) in changes.iter().enumerate() {
        let mut restored = value.clone();
        (change.restore)(&mut restored);
        if restored != *value
            && let Some(mut proof) = prove_pin_delta(&restored, expected, &changes[index + 1..])
        {
            proof.shape_changes.insert(0, change.name);
            return Some(proof);
        }
    }
    None
}

/// Assert `commits` digest to `expected`; a failure prints the digests the
/// turn produced.
pub(crate) fn assert_commit_pins(
    scenario: &str,
    commits: &[lash_core::RuntimeCommit],
    expected: &[&str],
) {
    let digests = commits.iter().map(commit_digest).collect::<Vec<_>>();
    #[expect(
        clippy::disallowed_methods,
        reason = "the opt-in fixture generator supplies its capture directory to this test host"
    )]
    let capture_directory = std::env::var_os("LASH_RUNTIME_COMMIT_PIN_CAPTURE_DIR");
    if let Some(directory) = capture_directory {
        assert_eq!(
            commits.len(),
            expected.len(),
            "{scenario}: commit count changed"
        );
        let proofs = commits
            .iter()
            .zip(expected)
            .map(|(commit, expected)| {
                let mut value = serde_json::to_value(commit).expect("a commit serializes");
                mask_run_variant(&mut value);
                prove_pin_delta(&value, expected, SHAPE_CHANGES).unwrap_or_else(|| {
                    panic!("{scenario}: named serialized-shape reversals do not reproduce pin {expected}; current digest {}", value_digest(&value))
                })
            })
            .collect::<Vec<_>>();
        let capture = serde_json::json!({
            "scenario": scenario,
            "expected": expected,
            "digests": digests,
            "proofs": proofs,
        });
        #[expect(
            clippy::disallowed_methods,
            reason = "the fixture regeneration host writes the test's captured pin evidence"
        )]
        let capture_written = std::fs::write(
            std::path::Path::new(&directory).join(format!("{}.json", scenario.replace(' ', "-"))),
            serde_json::to_vec_pretty(&capture).expect("a pin capture serializes"),
        );
        capture_written.expect("write the captured pin");
        return;
    }
    assert_eq!(digests, expected, "{scenario}: the committed bytes changed");
}

/// A runtime for a pinned turn, on a session of its own. Commit admission is
/// process-wide and keyed by session, so a pinned turn on a shared session id
/// could queue behind another suite's commit, and a cancelled one would then
/// refuse to wait.
pub(crate) async fn pinned_runtime(
    session_id: &str,
    plugins: Vec<std::sync::Arc<dyn lash_core::facade_support::PluginFactory>>,
    tools: std::sync::Arc<dyn lash_core::ToolProvider>,
    transport: lash_core::testing::TestProvider,
    host: lash_core::facade_support::EmbeddedRuntimeHost,
    store: std::sync::Arc<dyn lash_core::RuntimeStore>,
) -> lash_core::facade_support::LashRuntime {
    let backend = host.core.backend().clone();
    lash_core::testing::runtime_helpers::TestRuntime::new(&backend, transport)
        .plugins(plugins)
        .tools(tools)
        .host(host)
        .store(store)
        .with_session_id(lash_core::SessionId::fixture(session_id))
        .build()
        .await
}

#[test]
fn capture_proof_requires_all_named_deltas_and_refuses_unrelated_bytes() {
    let current = serde_json::json!({
        "config": {"model": {"model": {"metadata": {"limits": {
            "context_window_tokens": 200_000,
            "output_tokens": {"capacity": null, "default_cap": null}
        }}}}},
        "graph": {"Extend": {"nodes": [{
            "kind": "frame_open",
            "assignment": {"policy": {"autonomous": false}}
        }]}},
        "outcome": "completed"
    });
    let previous = serde_json::json!({
        "config": {"model": {"model": {"metadata": {"limits": {
            "context_window_tokens": 200_000
        }}}}},
        "graph": {"Extend": {"nodes": [{
            "kind": "frame_open",
            "assignment": {"policy": {"autonomous": false, "session_id": null}}
        }]}},
        "outcome": "completed"
    });
    let expected = value_digest(&previous);
    assert!(prove_pin_delta(&current, &expected, &SHAPE_CHANGES[..1]).is_none());
    assert!(prove_pin_delta(&current, &expected, &SHAPE_CHANGES[1..]).is_none());
    let proof = prove_pin_delta(&current, &expected, SHAPE_CHANGES).expect("both named deltas");
    assert_eq!(proof.shape_changes.len(), 2);
    assert_eq!(proof.restored_digest, expected);
    let unchanged = prove_pin_delta(&previous, &expected, SHAPE_CHANGES).expect("unchanged pin");
    assert!(unchanged.shape_changes.is_empty());

    let mut unrelated = current.clone();
    unrelated["outcome"] = serde_json::json!("cancelled");
    assert!(prove_pin_delta(&unrelated, &expected, SHAPE_CHANGES).is_none());
    let mut output_fact = current;
    output_fact["config"]["model"]["model"]["metadata"]["limits"]["output_tokens"]["capacity"] =
        serde_json::json!(100);
    assert!(prove_pin_delta(&output_fact, &expected, SHAPE_CHANGES).is_none());
}
