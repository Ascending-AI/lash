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
    store: std::sync::Arc<dyn lash_core::RuntimePersistence>,
) -> lash_core::facade_support::LashRuntime {
    let backend = host.core.backend().clone();
    lash_core::testing::runtime_helpers::TestRuntime::new(&backend, transport)
        .plugins(plugins)
        .tools(tools)
        .host(host)
        .store(store)
        .with_session_id(session_id)
        .build()
        .await
}
