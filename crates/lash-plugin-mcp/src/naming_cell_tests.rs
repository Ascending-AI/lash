use super::*;
use lashlang::testing::ast_builders as b;
use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    Value as CellValue,
};

fn catalog(names: &[&str]) -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(
        import_tools(
            "docs",
            names
                .iter()
                .map(|name| {
                    serde_json::from_value(json!({"name":name,"inputSchema":{"type":"object"}}))
                        .expect("tool")
                })
                .collect(),
            std::time::Duration::from_secs(30),
        )
        .expect("catalog imports")
        .into_values()
        .map(|tool| tool.definition)
        .collect(),
    )
}

fn environment(catalog: &lash_core::ToolCatalog) -> lashlang::LashlangHostEnvironment {
    lash_lashlang_runtime::lashlang_host_environment_from_tool_catalog(
        catalog,
        Default::default(),
        Default::default(),
    )
    .expect("MCP bindings import")
}

struct McpCellHost {
    resources: lashlang::LashlangHostCatalog,
    pool: Arc<McpConnectionPool>,
    dispatched: Mutex<Vec<String>>,
}

impl ExecutionHost for McpCellHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(call) => {
                let CellValue::Resource(receiver) = &call.receiver else {
                    panic!("module receiver")
                };
                let resolved = self
                    .resources
                    .resolve_module_operation(
                        &receiver.resource_type,
                        &receiver.alias,
                        &call.operation,
                    )
                    .expect("linked operation");
                self.dispatched
                    .lock_recover()
                    .push(resolved.host_operation.to_owned());
                let result = self
                    .pool
                    .call_tool_by_id(
                        &ToolId::from(resolved.host_operation),
                        &json!({}),
                        &lash_core::testing::mock_attempt_context(),
                    )
                    .await;
                assert!(result.is_success(), "{result:?}");
                Ok(AbilityOutcome::Value(lashlang::from_json(
                    result.value_for_projection(),
                )))
            }
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            other => panic!("unexpected operation {other:?}"),
        }
    }
}

async fn cell_calls_bare_and_hashed_paths(typescript: bool) {
    let peer = r#"
import json,sys
for line in sys.stdin:
    m=json.loads(line); method=m.get('method')
    if method=='initialize': result={'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'docs','version':'1'}}
    elif method=='tools/list': result={'tools':[{'name':n,'inputSchema':{'type':'object'}} for n in ['delete','search-docs','search_docs']]}
    elif method=='tools/call': result={'content':[{'type':'text','text':m['params']['name']}]}
    else: continue
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':result}),flush=True)
"#;
    let pool = McpConnectionPool::connect(BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig::stdio(McpStdioTransport::new(
            "python3",
            vec!["-u".into(), "-c".into(), peer.into()],
        )),
    )]))
    .await
    .expect("MCP peer connects");
    let catalog = lash_core::ToolCatalog::from_tool_definitions(pool.advertised_tools());
    assert!(
        catalog
            .tools
            .iter()
            .any(|tool| tool.manifest.name == "mcp__docs__delete")
    );
    let env = environment(&catalog);
    let host = McpCellHost {
        resources: env.resources.clone(),
        pool: Arc::clone(&pool),
        dispatched: Mutex::new(Vec::new()),
    };
    for (path, raw) in [
        ("docs.delete", "delete"),
        ("docs.search_docs__6rlrgooy", "search-docs"),
    ] {
        let linked = if typescript {
            lash_typescript::link(&format!("finish(await {path}({{}}));"), &env)
                .expect("TypeScript cell links")
        } else {
            let program = b::program(vec![b::finish(b::unwrap(b::receiver_call(
                b::resource(&["docs"]),
                path.strip_prefix("docs.").expect("module"),
                vec![b::record(Vec::new())],
            )))]);
            lashlang::LinkedModule::link(program, &env).expect("Lashlang cell links")
        };
        let result = lashlang::execute(
            &lashlang::testing::harness::compile_linked_main(&linked),
            &mut lashlang::State::new(),
            &host,
        )
        .await
        .expect("cell executes");
        assert_eq!(
            result,
            ExecutionOutcome::Finished(lashlang::from_json(
                json!({"content":[{"type":"text","text":raw}]})
            ))
        );
    }
    assert_eq!(
        *host.dispatched.lock_recover(),
        ["mcp:4:docs/6:delete", "mcp:4:docs/11:search-docs"]
    );
    pool.shutdown_all().await;
}

#[tokio::test]
async fn typescript_cell_calls_bare_and_hashed_mcp_paths() {
    cell_calls_bare_and_hashed_paths(true).await;
}

#[tokio::test]
async fn lashlang_cell_calls_bare_and_hashed_mcp_paths() {
    cell_calls_bare_and_hashed_paths(false).await;
}

#[test]
fn bare_then_remains_a_dialect_refusal() {
    let tools = catalog(&["then"]);
    assert_eq!(tools.tools[0].manifest.name, "mcp__docs__then");
    let refusal = lash_typescript::ensure_tool_call_path_addressable("docs.then")
        .expect_err("TypeScript owns refusal");
    assert_eq!(
        refusal.code,
        lash_typescript::DiagnosticCode::MethodUnsupported
    );
}

#[test]
fn a_collision_rename_invalidates_the_process_host_requirements() {
    let initial = environment(&catalog(&["search_docs"]));
    assert!(
        initial
            .resources
            .provides_module_operation("docs", "search_docs")
    );
    let b = b::module(
        vec![b::process(
            "main",
            Vec::new(),
            b::finish(b::receiver_call(
                b::resource(&["docs"]),
                "search_docs",
                vec![b::record(Vec::new())],
            )),
        )],
        Vec::new(),
    );
    let linked = lashlang::LinkedModule::link(b, &initial).expect("original process links");
    let refreshed = environment(&catalog(&["search_docs", "search-docs"]));
    let refusal = lash_lashlang_runtime::lashlang_host_environment_satisfies_requirements(
        linked.artifact.host_requirements(),
        &refreshed,
    )
    .expect_err("renamed process operation fails host admission");
    assert!(
        matches!(refusal, lash_lashlang_runtime::LashlangRuntimeError::ModuleOperationUnavailable { module, operation } if module == "docs" && operation == "search_docs")
    );
}
