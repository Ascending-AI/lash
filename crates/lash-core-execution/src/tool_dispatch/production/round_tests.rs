//! A parked call's resolution settles under the declaration its park
//! recorded.

use super::*;
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;

fn integer() -> lash_kernel_value::json_schema::JsonSchema {
    lash_kernel_value::json_schema::JsonSchema::admit(serde_json::json!({ "type": "integer" }))
        .expect("an integer schema is admitted")
}

/// The output of a call that parked under `declaration` on a process
/// whose terminal is a Finish with `value`.
fn resolved_finish(
    declaration: lash_vocabulary::tool_declaration::ToolDeclaration,
    value: serde_json::Value,
) -> ToolCallOutput {
    let owner = crate::EffectOpener::turn("session", "turn");
    let parked = ParkedCall {
        completion: crate::PendingCompletion {
            resolved_by: Some(crate::PendingResolver::ProcessTerminal {
                process_id: crate::process_id_for_test("finisher"),
            }),
            ..Default::default()
        },
        launch: None,
        declaration,
        output_schema: lash_kernel_value::json_schema::JsonSchema::any(),
    };
    let parked = Material::journal_local(
        MaterialOwner::Run {
            opener: owner.clone(),
        },
        MaterialRole::AttemptOutput,
        serde_json::to_string(&parked).expect("a parked call encodes"),
    )
    .parked("wait".to_owned());
    let terminal = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        ToolCallOutput::finish(crate::ToolValue::untrusted_json(value)),
    );
    let call = crate::sansio::PendingToolCall {
        call_id: crate::ToolCallId::fixture("parked-finish"),
        provider_call_id: None,
        tool_name: "submit".to_owned(),
        args: serde_json::json!({}),
        replay: None,
    };
    let resolution = Resolution::Ok(
        serde_json::to_value(crate::Durable(&terminal)).expect("a terminal encodes"),
    );
    let SettledOutput::Completed(material) = resolved_member(&owner, &call, &parked, resolution)
    else {
        panic!("a resolved park completes the call");
    };
    material
        .typed::<CompletedCall>()
        .expect("the completed call is held typed")
        .output
        .clone()
}

/// FIG-5823 law 1, deferred route: a Finish that arrives through a park's
/// resolution is checked against the value schema the park recorded. A
/// value it refuses fails the call with the typed mismatch and no control;
/// a value it admits settles carrying that schema as its witness.
#[test]
fn a_parked_finish_settles_under_the_declaration_its_park_recorded() {
    let declaration = lash_vocabulary::tool_declaration::ToolDeclaration {
        controls: lash_kernel_value::tool_declaration::TurnControls::finish(integer()),
        ..Default::default()
    };
    let refused = resolved_finish(declaration.clone(), serde_json::json!("seven"));
    assert!(refused.as_turn_control().is_none(), "{refused:?}");
    let crate::ToolCallOutcome::Failure(failure) = &refused.outcome else {
        panic!("a mismatched value fails the call: {refused:?}");
    };
    assert!(
        matches!(
            failure.cause.as_deref(),
            Some(crate::ToolFailureCause::Declaration {
                refusal: lash_vocabulary::tool_declaration::DeclarationRefusal::FinishValueMismatch { value_schema, .. },
            }) if **value_schema == integer()
        ),
        "{failure:?}"
    );

    let accepted = resolved_finish(declaration, serde_json::json!(7));
    assert!(
        matches!(
            accepted.as_turn_control(),
            Some(crate::TurnControl::Finish {
                value_schema: Some(schema),
                ..
            }) if schema == &integer()
        ),
        "{accepted:?}"
    );
}

fn observed_context() -> (
    crate::RuntimeExecutionContext<'static>,
    tokio::sync::mpsc::UnboundedReceiver<crate::TurnActivity>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut built = crate::testing::TestExecutionContextBuilder::over_controller(
        crate::ActorContext::unavailable(),
    )
    .build();
    Arc::get_mut(&mut built.dispatch)
        .expect("the fixture owns its dispatch")
        .observer = crate::testing::ChannelObservationSink::new(None, Some(tx));
    (built.into_runtime(), rx)
}

/// S1: a tool answered without admission has no measured execution window.
#[test]
fn a_tool_answered_without_admission_has_no_duration() {
    let (context, mut observed) = observed_context();
    let call = crate::sansio::PendingToolCall {
        call_id: crate::ToolCallId::fixture("unadmitted"),
        provider_call_id: None,
        tool_name: "unknown".into(),
        args: serde_json::Value::Null,
        replay: None,
    };
    ProductionToolHandlers::new(context).unavailable_call(&call);
    assert!(matches!(
        observed.try_recv().expect("the call starts").event,
        crate::TurnEvent::ToolCallStarted { .. }
    ));
    let crate::TurnEvent::ToolCallReturned { duration_ms, .. } =
        observed.try_recv().expect("the call answers").event
    else {
        panic!("a return follows its start")
    };
    assert_eq!(duration_ms, None);
}

/// T5: a refused empty batch has no declaration whose index or kind can be reported.
#[tokio::test]
async fn an_empty_invalid_version_batch_emits_only_a_batch_refusal() {
    let (context, mut observed) = observed_context();
    let call = crate::ToolCallId::fixture("batch-refused");
    let realization = crate::tool_dispatch::execute_final_tool_intents(
        &context.dispatch().intent_realization_context(),
        &call,
        &lash_execution_contracts::tool_intent::ToolIntents {
            protocol_version: 0,
            intents: Vec::new(),
        },
        None,
    )
    .await
    .expect("the protocol refusal is an outcome");
    context.emit_tool_intent_outcome_activities("batch", &call, &realization.receipt.outcomes);
    let activity = observed.try_recv().expect("the batch refusal is observed");
    assert!(
        matches!(&activity.event, crate::TurnEvent::ToolIntentOutcome { call_id, outcome: crate::ToolIntentActivityOutcome::BatchRefused {
        refusal: lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded: 0 },
    }} if call_id == &call)
    );
    let encoded = serde_json::to_value(activity.event).expect("the activity serializes");
    assert!(encoded["outcome"].get("intent_index").is_none());
    assert!(encoded["outcome"].get("kind").is_none());
    assert!(
        observed.try_recv().is_err(),
        "no per-intent activity was invented"
    );
}

/// OP-INTERNAL: a round cannot recover a hidden operation from its full
/// recorded provider routes after the protocol projects its callable catalog.
#[test]
fn an_operation_absent_from_the_callable_catalog_cannot_be_admitted_from_routes() {
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        "private",
        "dataframe.run",
        "private",
        serde_json::json!({}),
        serde_json::json!({"type":"null"}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(1));
    let entry = crate::SurfaceEntry::advertised(
        crate::testing::test_tools_provider(),
        definition.clone(),
        None,
    );
    let registry = Arc::new(crate::ToolRegistry::default());
    let routes = crate::SurfaceTools::of_entries([entry], registry);
    for visible in [false, true] {
        let catalog = crate::ToolCatalog::from_tool_definitions(if visible {
            vec![definition.clone()]
        } else {
            Vec::new()
        });
        let context = crate::testing::TestExecutionContextBuilder::over_controller(
            crate::ActorContext::unavailable(),
        )
        .routes(routes.clone())
        .tool_catalog(catalog)
        .build()
        .into_runtime();
        let handlers = ProductionToolHandlers::new(context);
        let call = crate::sansio::PendingToolCall {
            call_id: crate::ToolCallId::fixture("private"),
            provider_call_id: None,
            tool_name: "dataframe.run".into(),
            args: serde_json::json!({}),
            replay: None,
        };
        let invocation = ToolInvocation::from_pending(call, definition.manifest.id.clone());
        assert_eq!(handlers.leaf_definition(&invocation).is_some(), visible);
    }
}
