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

pub(crate) fn commit_digest(commit: &lash_core::RuntimeCommit) -> String {
    let mut value = serde_json::to_value(commit).expect("a runtime commit serializes");
    mask_run_variant(&mut value);
    let bytes = serde_json::to_vec(&value).expect("a masked commit serializes");
    lash_core::stable_hash::sha256_hex(&bytes)
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
        let restored_frame_digests = commits
            .iter()
            .map(|commit| {
                let frame =
                    commit.graph.nodes().iter().rev().find_map(|node| {
                        node.frame_open().map(|_| node.node_id.as_str().to_owned())
                    });
                let mut value = serde_json::to_value(commit).expect("a commit serializes");
                value
                    .as_object_mut()
                    .expect("a commit is an object")
                    .insert(
                        "current_frame_node_id".to_owned(),
                        serde_json::to_value(frame).expect("a frame id serializes"),
                    );
                mask_run_variant(&mut value);
                lash_core::stable_hash::sha256_hex(
                    &serde_json::to_vec(&value).expect("a masked commit serializes"),
                )
            })
            .collect::<Vec<_>>();
        let capture = serde_json::json!({
            "scenario": scenario,
            "expected": expected,
            "digests": digests,
            "restored_frame_digests": restored_frame_digests,
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
        if digests != expected {
            assert_eq!(
                restored_frame_digests, expected,
                "{scenario}: restoring only the removed frame claim must reproduce the old pin"
            );
        }
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
