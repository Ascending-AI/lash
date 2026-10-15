//! A cell's numbers reach a tool as JSON in the one spelling every lash path
//! writes them in (`Float::json_number`).

use super::*;

/// `search.run`: a tool that takes any record and keeps the arguments it
/// was called with.
#[derive(Default)]
struct KeepsArgs {
    received: std::sync::Mutex<Option<serde_json::Value>>,
}

fn search() -> lash_core::ToolDefinition {
    definition("tool:search_run", "search_run", "search", "run")
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for KeepsArgs {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(vec![search().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        matches!(name, "search_run" | "tool:search_run").then(|| Arc::new(search().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        *self.received.lock().expect("received arguments") = Some(call.args.clone());
        lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::ok(
            serde_json::json!("ok"),
        ))
    }
}

/// FIG-6290: a whole number below 2^53 reaches a tool as a JSON integer, in
/// either dialect, so a tool that types the field as `u64` reads it. Any
/// other float stays a float: `2.5`, and TypeScript's `2 ** 53`, which also
/// stands for integers a float cannot hold. A Python integer is exact at any
/// size, and a Python whole float is the same JSON value as its integer.
#[tokio::test(flavor = "multi_thread")]
async fn a_whole_number_reaches_a_tool_as_a_json_integer() {
    let cases = [
        (
            CellDialect::typescript(),
            "await search.run({ limit: 5, half: 2.5, edge: 2 ** 53 });",
            [
                ("limit", "5"),
                ("half", "2.5"),
                ("edge", "9007199254740992.0"),
            ],
        ),
        (
            CellDialect::python(),
            "await search_run({'limit': 5.0, 'half': 2.5, 'edge': 2**53})",
            [
                ("limit", "5"),
                ("half", "2.5"),
                ("edge", "9007199254740992"),
            ],
        ),
    ];
    for (dialect, code, expected) in cases {
        let tools = Arc::new(KeepsArgs::default());
        let host = open_host().await;
        let services = if dialect.name() == CellDialect::python().name() {
            cell_services(&dialect, python_workers())
        } else {
            cell_services(&dialect, lash_vm_client::service::Service::default())
        };
        let response = run_cell(
            &mut crate::testing::session_state(&dialect),
            cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
            &services,
            code,
        )
        .await;
        assert!(response.error().is_none(), "{:?}", response.error());
        let args = tools
            .received
            .lock()
            .expect("received arguments")
            .clone()
            .expect("the tool ran");
        for (field, text) in expected {
            assert_eq!(
                args[field].to_string(),
                text,
                "{} `{field}`",
                dialect.name()
            );
        }
        assert_eq!(
            serde_json::from_value::<u64>(args["limit"].clone()).ok(),
            Some(5)
        );
    }
}
