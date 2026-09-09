use super::*;

// The FIG-2764 witness: the ticket's `await [retail.order({ id: id })? for id
// in ids]` cell, run end to end through the RLM session against a scripted
// retail-like module, must print the filtered delivered orders exactly as the
// imperative loop does.

struct RetailTools;

#[async_trait]
impl ToolProvider for RetailTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![retail_order_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "retail_order").then(|| Arc::new(retail_order_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        let id = call
            .args
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let status = if id.ends_with('7') {
            "delivered"
        } else {
            "shipped"
        };
        lash_core::ToolOutcome::ok(serde_json::json!({ "id": id, "status": status }))
    }
}

fn retail_order_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:retail_order",
        "retail_order",
        "Look up one retail order by id.",
        serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["retail"], "order"))
}

const TICKET_CELL: &str = r#"
ids = ["o-17", "o-22", "o-37"]
orders = await [retail.order({ id: id })? for id in ids]
delivered = [o.id for o in orders if o.status == "delivered"]
print(format("delivered: {}", delivered))
"#;

const IMPERATIVE_CELL: &str = r#"
ids = ["o-17", "o-22", "o-37"]
delivered = []
for id in ids {
  o = await retail.order({ id: id })?
  if o.status == "delivered" {
    delivered = push(delivered, o.id)
  }
}
print(format("delivered: {}", delivered))
"#;

async fn delivered_orders_printed_by(cell: &str) -> Result<(String, usize)> {
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let captured = Arc::clone(&requests);
    let cells = Arc::new(TokioMutex::new(VecDeque::from(vec![
        lashlang_block(cell),
        lashlang_block(r#"finish "done""#),
    ])));
    let provider = crate::testing::TestProvider::builder()
        .kind("fig2764-comprehension")
        .complete(move |request| {
            let captured = Arc::clone(&captured);
            let cells = Arc::clone(&cells);
            async move {
                captured.lock_recover().push(
                    serde_json::to_string(&request.messages).expect("serialize request messages"),
                );
                let text = cells.lock().await.pop_front().expect("scripted cell");
                Ok(text_response(&text))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(provider)
        .model(mock_model_spec())
        .tools(Arc::new(RetailTools))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("fig2764-comprehension").open().await?;
    let events = RecordingEvents::default();
    let result = session
        .turn(TurnInput::text("which orders were delivered?"))
        .stream_to(&events)
        .await?;
    assert!(
        matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
        ),
        "the cell must finish, got {:?}",
        result.outcome
    );
    let tool_calls = events
        .snapshot()
        .await
        .into_iter()
        .filter(|event| matches!(event.event, TurnEvent::ToolCallCompleted { .. }))
        .count();
    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2, "the scripted cell then `finish`");
    // The cell's own source echoes the format string; the print output is the
    // last `delivered: ` in the request, in the history output that follows.
    let printed = requests[1]
        .rsplit("delivered: ")
        .next()
        .filter(|rest| !rest.starts_with("{}"))
        .map(|rest| rest.split("\\n").next().unwrap_or(rest).to_string())
        .unwrap_or_else(|| {
            panic!(
                "the print output must reach the next request: {}",
                requests[1]
            )
        });
    Ok((printed, tool_calls))
}

#[tokio::test]
async fn awaited_comprehension_of_unwrapped_calls_matches_the_imperative_loop() -> Result<()> {
    let (comprehension, comprehension_calls) = delivered_orders_printed_by(TICKET_CELL).await?;
    let (imperative, imperative_calls) = delivered_orders_printed_by(IMPERATIVE_CELL).await?;

    assert_eq!(
        comprehension, imperative,
        "`await [call(x)? for x in xs]` must print what the imperative loop prints"
    );
    assert!(
        comprehension.contains("o-17") && comprehension.contains("o-37"),
        "the delivered orders are the values, not result records: {comprehension}"
    );
    assert!(
        !comprehension.contains("o-22") && !comprehension.contains("\"ok\""),
        "filtering by status must see unwrapped orders: {comprehension}"
    );
    assert_eq!(comprehension_calls, 3, "every order id is fetched once");
    assert_eq!(imperative_calls, 3);
    Ok(())
}
