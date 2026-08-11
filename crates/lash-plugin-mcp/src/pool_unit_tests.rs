use super::*;
use lash_core::ToolProvider;
use lash_sansio::sync::{MutexExt, RwLockExt};

#[tokio::test]
async fn roots_notification_failures_are_aggregated_after_every_attempt() {
    let attempts = Arc::new(AtomicU64::new(0));
    let failures = collect_notification_failures(
        vec![
            ("alpha".to_string(), Some("offline")),
            ("bravo".to_string(), None),
            ("charlie".to_string(), Some("closed")),
        ],
        |failure| {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                failure.map_or(Ok(()), Err)
            }
        },
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(failures, ["`alpha`: offline", "`charlie`: closed"]);
}

/// Regression for the header-drop bug: custom/auth headers configured for
/// an HTTP MCP server must be translated into the `http` header types the
/// transport actually sends. Before the fix, `connect_service` called
/// `from_uri` and dropped the configured `headers` map entirely, so an
/// `Authorization` header never reached the server.
#[test]
fn build_http_headers_carries_configured_headers() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer secret-token".to_string(),
    );
    headers.insert("X-Tenant".to_string(), "acme".to_string());

    let built = build_http_headers("api", &headers).expect("valid headers convert");

    assert_eq!(
        built
            .get(&HeaderName::from_static("authorization"))
            .map(|v| v.to_str().unwrap()),
        Some("Bearer secret-token"),
        "configured Authorization header must be carried through to the transport"
    );
    assert_eq!(
        built
            .get(&HeaderName::from_static("x-tenant"))
            .map(|v| v.to_str().unwrap()),
        Some("acme")
    );
    assert_eq!(built.len(), 2);
}

#[test]
fn build_http_headers_empty_map_is_empty() {
    let built = build_http_headers("api", &BTreeMap::new()).expect("empty converts");
    assert!(built.is_empty());
}

#[test]
fn build_http_headers_rejects_malformed_name() {
    let mut headers = BTreeMap::new();
    headers.insert("Bad Header Name".to_string(), "x".to_string());
    let err = build_http_headers("api", &headers).expect_err("malformed name rejected");
    assert!(
        matches!(err, McpError::Config(_)),
        "expected a config error, got {err:?}"
    );
}

#[test]
fn build_http_headers_rejects_malformed_value() {
    let mut headers = BTreeMap::new();
    // A newline is not a legal header value byte.
    headers.insert("X-Bad".to_string(), "line1\nline2".to_string());
    let err = build_http_headers("api", &headers).expect_err("malformed value rejected");
    assert!(
        matches!(err, McpError::Config(_)),
        "expected a config error, got {err:?}"
    );
}

/// A server that is down at startup must not fail pool construction: the
/// entry stays registered (status: disconnected, with the error recorded)
/// and only configuration errors abort.
#[tokio::test]
async fn connect_tolerates_unreachable_server() {
    let mut servers = BTreeMap::new();
    servers.insert(
        "down".to_string(),
        McpServerConfig::Stdio {
            // Spawns, says nothing, exits — the handshake fails fast.
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 1".to_string()],
            env: BTreeMap::new(),
            cwd: None,
            startup_timeout_ms: 1_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 1_000,
                ..Default::default()
            },
            binary_content_attachments: false,
        },
    );

    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("an unreachable server must not fail pool construction");

    assert!(pool.advertised_tools().is_empty());
    let statuses = pool.server_statuses();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].server_name, "down");
    assert!(!statuses[0].connected);
    assert!(
        statuses[0].last_error.is_some(),
        "the connection failure is recorded for observability"
    );

    let result = pool
        .call_tool(
            "mcp__down__anything",
            &json!({}),
            &lash_core::testing::mock_tool_context(),
        )
        .await;
    assert!(!result.is_success(), "calls fail loudly while disconnected");

    pool.shutdown_all().await;

    let result = pool
        .call_tool(
            "mcp__down__anything",
            &json!({}),
            &lash_core::testing::mock_tool_context(),
        )
        .await;
    let output = result
        .as_done_output()
        .expect("post-shutdown call must complete with a failure");
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("post-shutdown call must be a structured failure: {output:?}");
    };
    assert_eq!(failure.class, ToolFailureClass::Unavailable);
    assert_eq!(failure.code, "mcp_pool_shut_down");
    assert_eq!(failure.retry, ToolRetryDisposition::Never);
}

#[tokio::test]
async fn connect_rejects_server_names_with_the_same_normalized_prefix() {
    let servers = BTreeMap::from([
        (
            "Foo".to_string(),
            McpServerConfig::stdio("sh", vec!["-c".to_string(), "exit 1".to_string()]),
        ),
        (
            "foo".to_string(),
            McpServerConfig::stdio("sh", vec!["-c".to_string(), "exit 1".to_string()]),
        ),
    ]);

    let error = match McpConnectionPool::connect(servers).await {
        Err(error) => error,
        Ok(pool) => {
            pool.shutdown_all().await;
            panic!("colliding normalized server prefixes must be a configuration error");
        }
    };
    let message = error.to_string();
    assert!(message.contains("`Foo`"), "{message}");
    assert!(message.contains("`foo`"), "{message}");
    assert!(message.contains("prefix `foo`"), "{message}");
}

struct NativeAndMcpProvider {
    native: ToolDefinition,
    mcp: crate::McpToolProvider,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for NativeAndMcpProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        let mut manifests = vec![self.native.manifest()];
        manifests.extend(self.mcp.tool_manifests());
        manifests
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        if name == self.native.name() {
            return Some(Arc::new(self.native.contract()));
        }
        self.mcp.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolResult {
        if call.name == self.native.name() {
            return ToolResult::ok(json!("native-ok"));
        }
        self.mcp.execute(call).await
    }
}

#[cfg(unix)]
#[tokio::test]
async fn colliding_attach_cannot_kill_native_tools_during_catalog_rebuild() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "catalog", "version": "1.0.0" }
        }
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": [{
                "name": "lookup",
                "inputSchema": { "type": "object" }
            }]
        }
    });
    let config = || McpServerConfig::Stdio {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            "read -r _; printf '%s\\n' \"$INITIALIZE\"; read -r _; \
             read -r _; printf '%s\\n' \"$LIST\"; cat >/dev/null"
                .to_string(),
        ],
        env: BTreeMap::from([
            ("INITIALIZE".to_string(), initialize.to_string()),
            ("LIST".to_string(), list.to_string()),
        ]),
        cwd: None,
        startup_timeout_ms: 2_000,
        call_policy: McpCallPolicy::default(),
        binary_content_attachments: false,
    };
    let pool = McpConnectionPool::connect(BTreeMap::from([("Docs".to_string(), config())]))
        .await
        .expect("connect the original MCP server");

    let attach_result = pool.attach("docs".to_string(), config()).await;
    let native = ToolDefinition::raw(
        "tool:native/status",
        "native_status",
        "native status",
        ToolDefinition::default_input_schema(),
        json!({ "type": "string" }),
    );
    let native_id = native.manifest.id.clone();
    let rebuilt = lash_core::ToolRegistry::from_tool_provider(Arc::new(NativeAndMcpProvider {
        native,
        mcp: crate::McpToolProvider::new(Arc::clone(&pool)),
    }));

    // Reap every stdio child before making assertions that can panic. With the
    // reservation reverted, the rejected attach above becomes a second child.
    pool.shutdown_all().await;

    let registry =
        rebuilt.expect("a rejected collision must not kill the mixed native/MCP catalog");
    let manifests = registry.tool_manifests();
    assert_eq!(manifests.len(), 2, "native and original MCP tools survive");
    assert!(
        manifests
            .iter()
            .any(|manifest| manifest.name == "native_status"),
        "the native tool remains in the rebuilt catalog"
    );
    assert!(
        manifests
            .iter()
            .any(|manifest| manifest.name == "mcp__docs__lookup"),
        "the original MCP tool remains in the rebuilt catalog"
    );
    let native_result = registry
        .execute_by_id(
            &native_id,
            &json!({}),
            &lash_core::testing::mock_tool_context(),
        )
        .await;
    assert_eq!(native_result.value_for_projection(), json!("native-ok"));

    let error = attach_result.expect_err("the colliding runtime attach must be rejected");
    assert!(matches!(error, McpError::Config(_)), "{error:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn eager_connects_start_in_parallel() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let alpha_marker = scratch.path().join("alpha.started");
    let bravo_marker = scratch.path().join("bravo.started");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "parallel", "version": "1.0.0" }
        }
    });
    let list = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } });
    let config = |own: &std::path::Path, other: &std::path::Path| McpServerConfig::Stdio {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            ": > \"$OWN\"; while [ ! -e \"$OTHER\" ]; do sleep 0.01; done; \
             read -r _; printf '%s\\n' \"$INITIALIZE\"; read -r _; \
             read -r _; printf '%s\\n' \"$LIST\"; cat >/dev/null"
                .to_string(),
        ],
        env: BTreeMap::from([
            ("OWN".to_string(), own.display().to_string()),
            ("OTHER".to_string(), other.display().to_string()),
            ("INITIALIZE".to_string(), initialize.to_string()),
            ("LIST".to_string(), list.to_string()),
        ]),
        cwd: None,
        startup_timeout_ms: 1_000,
        call_policy: McpCallPolicy::default(),
        binary_content_attachments: false,
    };
    let pool = McpConnectionPool::connect(BTreeMap::from([
        ("alpha".to_string(), config(&alpha_marker, &bravo_marker)),
        ("bravo".to_string(), config(&bravo_marker, &alpha_marker)),
    ]))
    .await
    .expect("parallel eager connects complete");

    assert!(
        pool.server_statuses().iter().all(|status| status.connected),
        "both handshakes require their peer child to have started"
    );
    pool.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn tools_list_changed_refreshes_the_live_catalog() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": "live", "version": "1.0.0" }
        }
    });
    let first_list = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": { "tools": [{ "name": "old-tool", "inputSchema": { "type": "object" } }] }
    });
    let second_list = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": { "tools": [{ "name": "new-tool", "inputSchema": { "type": "object" } }] }
    });
    let notification = json!({
        "jsonrpc": "2.0", "method": "notifications/tools/list_changed"
    });
    let script = "\
        read -r _; printf '%s\\n' \"$INITIALIZE\"; \
        read -r _; read -r _; printf '%s\\n' \"$FIRST_LIST\"; \
        printf '%s\\n' \"$NOTIFICATION\"; \
        read -r _; printf '%s\\n' \"$SECOND_LIST\"; cat >/dev/null";
    let pool = McpConnectionPool::connect(BTreeMap::from([(
        "live".to_string(),
        McpServerConfig::Stdio {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: BTreeMap::from([
                ("INITIALIZE".to_string(), initialize.to_string()),
                ("FIRST_LIST".to_string(), first_list.to_string()),
                ("SECOND_LIST".to_string(), second_list.to_string()),
                ("NOTIFICATION".to_string(), notification.to_string()),
            ]),
            cwd: None,
            startup_timeout_ms: 2_000,
            call_policy: McpCallPolicy::default(),
            binary_content_attachments: false,
        },
    )]))
    .await
    .expect("connect list-changing server");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let names = pool
                .advertised_tools()
                .into_iter()
                .map(|tool| tool.name().to_string())
                .collect::<Vec<_>>();
            if names == ["mcp__live__new_tool"] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tools/list_changed refreshes discovery");
    pool.shutdown_all().await;
}

#[tokio::test]
async fn attach_registers_an_outage_and_retries_like_initial_connect() {
    let pool = Arc::new(McpConnectionPool::empty());
    pool.attach(
        "down".to_string(),
        McpServerConfig::stdio("sh", vec!["-c".to_string(), "exit 1".to_string()]),
    )
    .await
    .expect("startup outage is registered rather than rejected");

    let statuses = pool.server_statuses();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].server_name, "down");
    assert!(!statuses[0].connected);
    assert!(statuses[0].last_error.is_some());
    pool.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn attach_reaps_the_previous_child_before_starting_its_replacement() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let old_pid = scratch.path().join("old.pid");
    let overlap = scratch.path().join("overlap");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "replace", "version": "1.0.0" }
        }
    });
    let list = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } });
    let handshake = "read -r _; printf '%s\\n' \"$INITIALIZE\"; \
                     read -r _; read -r _; printf '%s\\n' \"$LIST\"; cat >/dev/null";
    let config = |script: String, env: BTreeMap<String, String>| McpServerConfig::Stdio {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), script],
        env,
        cwd: None,
        startup_timeout_ms: 2_000,
        call_policy: McpCallPolicy::default(),
        binary_content_attachments: false,
    };
    let common_env = || {
        BTreeMap::from([
            ("INITIALIZE".to_string(), initialize.to_string()),
            ("LIST".to_string(), list.to_string()),
            ("OLD_PID".to_string(), old_pid.display().to_string()),
            ("OVERLAP".to_string(), overlap.display().to_string()),
        ])
    };
    let pool = McpConnectionPool::connect(BTreeMap::from([(
        "replace".to_string(),
        config(
            format!("printf '%s\\n' \"$$\" > \"$OLD_PID\"; {handshake}"),
            common_env(),
        ),
    )]))
    .await
    .expect("connect original child");
    assert!(old_pid.exists(), "original child records its pid");

    pool.attach(
        "replace".to_string(),
        config(
            format!(
                "if kill -0 \"$(cat \"$OLD_PID\")\" 2>/dev/null; then : > \"$OVERLAP\"; fi; {handshake}"
            ),
            common_env(),
        ),
    )
    .await
    .expect("attach replacement child");

    assert!(
        !overlap.exists(),
        "replacement must start only after the previous child is reaped"
    );
    pool.shutdown_all().await;
}

#[test]
fn known_protocol_errors_are_not_connection_loss() {
    assert!(!is_connection_loss(&ServiceError::UnexpectedResponse));
    assert!(!is_connection_loss(&ServiceError::Cancelled {
        reason: None
    }));
    assert!(!is_connection_loss(&ServiceError::Timeout {
        timeout: Duration::from_secs(1),
    }));
    assert!(is_connection_loss(&ServiceError::TransportClosed));
}

#[cfg(all(unix, feature = "lashlang"))]
#[tokio::test]
async fn normalization_collisions_dispatch_stably_across_respawn() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let respawn_marker = scratch.path().join("respawned");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "collision", "version": "1.0.0" }
        }
    });
    let first_list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "tools": [
            { "name": "get-user", "inputSchema": { "type": "object" } },
            { "name": "get_user", "inputSchema": { "type": "object" } }
        ] }
    });
    let respawn_list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "tools": [
            { "name": "get_user", "inputSchema": { "type": "object" } },
            { "name": "get-user", "inputSchema": { "type": "object" } }
        ] }
    });
    let hyphen_call = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": { "content": [{ "type": "text", "text": "hyphen" }] }
    });
    let underscore_call = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": { "content": [{ "type": "text", "text": "underscore" }] }
    });
    let script = "\
        read -r _; printf '%s\\n' \"$INITIALIZE\"; \
        read -r _; \
        read -r _; \
        if [ -e \"$RESPAWN_MARKER\" ]; then printf '%s\\n' \"$RESPAWN_LIST\"; \
        else : > \"$RESPAWN_MARKER\"; printf '%s\\n' \"$FIRST_LIST\"; fi; \
        read -r first_call; \
        case \"$first_call\" in *'\"name\":\"get-user\"'*) printf '%s\\n' \"$HYPHEN_CALL\";; \
        *) printf '%s\\n' \"$UNDERSCORE_CALL\";; esac";
    let servers = BTreeMap::from([(
        "directory".to_string(),
        McpServerConfig::Stdio {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: BTreeMap::from([
                ("INITIALIZE".to_string(), initialize.to_string()),
                ("FIRST_LIST".to_string(), first_list.to_string()),
                ("RESPAWN_LIST".to_string(), respawn_list.to_string()),
                ("HYPHEN_CALL".to_string(), hyphen_call.to_string()),
                ("UNDERSCORE_CALL".to_string(), underscore_call.to_string()),
                (
                    "RESPAWN_MARKER".to_string(),
                    respawn_marker.display().to_string(),
                ),
            ]),
            cwd: None,
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 2_000,
                ..Default::default()
            },
            binary_content_attachments: false,
        },
    )]);
    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("connect collision server");

    async fn dispatch(pool: &McpConnectionPool, operation: &str) -> Option<lash_core::ToolResult> {
        let definition = pool.advertised_tools().into_iter().find(|definition| {
            lash_lashlang_runtime::tool_lashlang_binding(&definition.manifest)
                .ok()
                .flatten()
                .and_then(|binding| binding.operation)
                .as_deref()
                == Some(operation)
        })?;
        Some(
            pool.call_tool(
                definition.name(),
                &json!({}),
                &lash_core::testing::mock_tool_context(),
            )
            .await,
        )
    }

    let first = dispatch(&pool, "get_user")
        .await
        .expect("base Lashlang operation is available before respawn");
    assert_eq!(first.value_for_projection(), json!("hyphen"));

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let Some(result) = dispatch(&pool, "get_user_2").await else {
            panic!("missing uniquified Lashlang operation `get_user_2` after respawn");
        };
        if result.is_success() {
            assert_eq!(result.value_for_projection(), json!("underscore"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server did not respawn before deadline: {result:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    pool.shutdown_all().await;
}

/// The first eager attempt fails, then a background reconnect spawns a live
/// child and blocks in discovery. Shutdown must cancel that in-progress
/// service explicitly and wait for the reconnect loop before returning;
/// keeping `pool` alive proves this does not rely on pool drop.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_all_reaps_child_from_in_progress_reconnect_before_return() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let attempt_file = scratch.path().join("attempted");
    let pid_file = scratch.path().join("mcp.pid");
    let discovery_file = scratch.path().join("discovering");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "reconnect-race", "version": "1.0.0" }
        }
    });
    let script = "\
            if [ ! -e \"$ATTEMPT_FILE\" ]; then : > \"$ATTEMPT_FILE\"; exit 1; fi; \
            printf '%s\\n' \"$$\" > \"$PID_FILE\"; \
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; : > \"$DISCOVERY_FILE\"; \
            cat >/dev/null"
        .to_string();
    let servers = BTreeMap::from([(
        "race".to_string(),
        McpServerConfig::Stdio {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            env: BTreeMap::from([
                (
                    "ATTEMPT_FILE".to_string(),
                    attempt_file.display().to_string(),
                ),
                ("PID_FILE".to_string(), pid_file.display().to_string()),
                (
                    "DISCOVERY_FILE".to_string(),
                    discovery_file.display().to_string(),
                ),
                ("RESP1".to_string(), initialize.to_string()),
            ]),
            cwd: None,
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 10_000,
                ..Default::default()
            },
            binary_content_attachments: false,
        },
    )]);
    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("startup outage keeps the pool alive");

    tokio::time::timeout(Duration::from_secs(10), async {
        while !discovery_file.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background reconnect reaches discovery");
    let pid = std::fs::read_to_string(&pid_file)
        .expect("read reconnect child pid")
        .trim()
        .to_string();
    assert!(
        process_exists(&pid),
        "reconnect child must be live before shutdown"
    );

    pool.shutdown_all().await;

    assert!(
        !process_exists(&pid),
        "shutdown_all must reap the in-progress reconnect child before returning"
    );
    assert!(pool.shut_down.load(Ordering::SeqCst));
}

#[cfg(unix)]
fn process_exists(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("probe child process")
        .success()
}

#[tokio::test]
async fn shutdown_all_wakes_actor_sleeping_until_keepalive() {
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = McpEntry::new(
        "keepalive".to_string(),
        McpServerConfig::Stdio {
            command: "unused".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            startup_timeout_ms: 1_000,
            call_policy: McpCallPolicy {
                liveness_probe_interval_ms: 60_000,
                ..Default::default()
            },
            binary_content_attachments: false,
        },
        McpHostServices::default(),
    );
    assert!(
        pool.install("keepalive".to_string(), Arc::clone(&entry))
            .is_ok()
    );
    tokio::time::timeout(Duration::from_secs(1), pool.shutdown_all())
        .await
        .expect("shutdown must wake keepalive instead of waiting for its interval");
    assert!(entry.actor_handle.lock_recover().is_none());
}

/// A connection that dies mid-life is detected on the next call and
/// re-established by the background reconnect loop; tool definitions are
/// kept across the outage so the surface stays stable.
#[tokio::test]
async fn pool_reconnects_after_transport_death() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "demo", "version": "1.0.0" }
        }
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": [{
                "name": "ping",
                "description": "Ping",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    });
    let call = json!({ "jsonrpc": "2.0", "id": 2, "result": { "content": [{ "type": "text", "text": "pong" }] } });

    // Serve initialize, tools/list, and exactly one tools/call, then exit:
    // the transport dies after the first successful call. Every reconnect
    // runs the same script again (rmcp request ids restart per connection).
    let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            read -r _; printf '%s\\n' \"$RESP3\""
        .to_string();

    let mut env = BTreeMap::new();
    env.insert("RESP1".to_string(), initialize.to_string());
    env.insert("RESP2".to_string(), list.to_string());
    env.insert("RESP3".to_string(), call.to_string());

    let mut servers = BTreeMap::new();
    servers.insert(
        "flaky".to_string(),
        McpServerConfig::Stdio {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            env,
            cwd: None,
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 2_000,
                timeout_disconnect_policy: TimeoutDisconnectPolicy::Never,
                ..Default::default()
            },
            binary_content_attachments: false,
        },
    );

    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("connects to the mock");
    let ctx = lash_core::testing::mock_tool_context();
    let args = json!({});

    let first = pool.call_tool("mcp__flaky__ping", &args, &ctx).await;
    assert!(first.is_success(), "first call succeeds: {first:?}");

    // The mock exited after the first call. Definitions must survive the
    // outage, and calls must fail until the reconnect loop brings the
    // (respawned) server back, after which calls succeed again.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut recovered = false;
    while std::time::Instant::now() < deadline {
        assert_eq!(
            pool.advertised_tools().len(),
            1,
            "tool definitions are kept across a disconnect"
        );
        let result = pool.call_tool("mcp__flaky__ping", &args, &ctx).await;
        if result.is_success() {
            recovered = true;
            break;
        }
        let output = result
            .as_done_output()
            .expect("a disconnected MCP call must complete with a failure");
        let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
            panic!("a disconnected MCP call must be a structured failure: {output:?}");
        };
        assert_eq!(failure.class, ToolFailureClass::Unavailable);
        assert!(
            matches!(
                failure.code.as_str(),
                "mcp_connection_lost" | "mcp_server_unavailable"
            ),
            "unexpected unavailable code: {}",
            failure.code
        );
        assert_eq!(
            failure.retry,
            ToolRetryDisposition::Safe {
                after_ms: Some(500)
            }
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(recovered, "pool must reconnect after the transport died");

    pool.shutdown_all().await;
}

#[tokio::test]
async fn call_timeout_is_a_typed_retryable_failure() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "demo", "version": "1.0.0" }
        }
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": [{
                "name": "hang",
                "description": "Never returns",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    });
    let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            read -r _; \
            cat >/dev/null"
        .to_string();
    let servers = BTreeMap::from([(
        "slow".to_string(),
        McpServerConfig::Stdio {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            env: BTreeMap::from([
                ("RESP1".to_string(), initialize.to_string()),
                ("RESP2".to_string(), list.to_string()),
            ]),
            cwd: None,
            startup_timeout_ms: 1_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 50,
                timeout_disconnect_policy: TimeoutDisconnectPolicy::Never,
                liveness_probe_timeout_ms: 50,
                ..Default::default()
            },
            binary_content_attachments: false,
        },
    )]);
    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("connect to hanging mock");

    let result = pool
        .call_tool(
            "mcp__slow__hang",
            &json!({}),
            &lash_core::testing::mock_tool_context(),
        )
        .await;
    let output = result
        .as_done_output()
        .expect("timeout must complete with a failure");
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("timeout must be a structured failure: {output:?}");
    };
    assert_eq!(failure.class, ToolFailureClass::Timeout);
    assert_eq!(failure.code, "mcp_call_timeout");
    assert_eq!(failure.retry, ToolRetryDisposition::Safe { after_ms: None });

    pool.shutdown_all().await;
}

/// Regression for the missing discovery timeout: a server that completes
/// the handshake but then hangs on `tools/list` must surface a
/// `StartupTimeout` rather than blocking `connect` forever.
#[tokio::test]
async fn discovery_hang_surfaces_startup_timeout() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "demo", "version": "1.0.0" }
        }
    });

    // Respond to `initialize`, swallow `notifications/initialized`, read the
    // `tools/list` request line, then hang (never respond) by blocking on
    // stdin. The short startup timeout must trip.
    let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; \
            cat >/dev/null"
        .to_string();

    let mut env = BTreeMap::new();
    env.insert("RESP1".to_string(), initialize.to_string());

    let config = McpServerConfig::Stdio {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), script],
        env,
        cwd: None,
        startup_timeout_ms: 750,
        call_policy: McpCallPolicy {
            call_timeout_ms: 10_000,
            ..Default::default()
        },
        binary_content_attachments: false,
    };

    let entry = McpEntry::new("hangs".to_string(), config, McpHostServices::default());
    match entry.establish().await {
        Err(McpError::StartupTimeout { .. }) => {}
        Err(other) => panic!("expected StartupTimeout from a hung tools/list, got {other:?}"),
        Ok(_) => panic!("a hung tools/list must not connect"),
    }
    assert!(entry.service_snapshot().is_none());
    assert!(
        entry
            .last_error
            .read_recover()
            .as_deref()
            .is_some_and(|err| err.contains("timed out") || err.contains("timeout")),
        "the failure is recorded for status reporting"
    );
}

/// Regression for accidentally serializing calls behind lifecycle state: two
/// concurrent `tools/call` requests to the same server must be able to be
/// in flight at once. The mock refuses to answer the first call until it
/// has read the second request line, so a serializing implementation (lock
/// held across `.await`) would deadlock and time out, while the concurrent
/// implementation completes both calls.
#[tokio::test]
async fn concurrent_calls_are_not_serialized_by_the_service_mutex() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "demo", "version": "1.0.0" }
        }
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": [{
                "name": "ping",
                "description": "Ping",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    });
    // rmcp assigns request ids 2 and 3 to the two concurrent calls. The
    // mock reads BOTH request lines before emitting EITHER response, which
    // is only possible if both requests are in flight concurrently.
    let call2 = json!({ "jsonrpc": "2.0", "id": 2, "result": { "content": [{ "type": "text", "text": "pong" }] } });
    let call3 = json!({ "jsonrpc": "2.0", "id": 3, "result": { "content": [{ "type": "text", "text": "pong" }] } });

    let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            read -r _; \
            read -r _; \
            printf '%s\\n' \"$RESP3\"; \
            printf '%s\\n' \"$RESP4\"; \
            cat >/dev/null"
        .to_string();

    let mut env = BTreeMap::new();
    env.insert("RESP1".to_string(), initialize.to_string());
    env.insert("RESP2".to_string(), list.to_string());
    env.insert("RESP3".to_string(), call2.to_string());
    env.insert("RESP4".to_string(), call3.to_string());

    let mut servers = BTreeMap::new();
    servers.insert(
        "svc".to_string(),
        McpServerConfig::Stdio {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            env,
            cwd: None,
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 5_000,
                ..Default::default()
            },
            binary_content_attachments: false,
        },
    );

    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("connects to concurrency mock");

    let ctx = lash_core::testing::mock_tool_context();
    let args = json!({});
    let (a, b) = tokio::join!(
        pool.call_tool("mcp__svc__ping", &args, &ctx),
        pool.call_tool("mcp__svc__ping", &args, &ctx),
    );
    assert!(a.is_success(), "first concurrent call failed: {a:?}");
    assert!(b.is_success(), "second concurrent call failed: {b:?}");

    pool.shutdown_all().await;
}
