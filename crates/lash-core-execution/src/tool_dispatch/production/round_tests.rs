//! A parked call's resolution settles under the declaration its park
//! recorded.

use super::*;

fn integer() -> crate::JsonSchema {
    crate::JsonSchema::admit(serde_json::json!({ "type": "integer" }))
        .expect("an integer schema is admitted")
}

/// The output of a call that parked under `declaration` on a process
/// whose terminal is a Finish with `value`.
fn resolved_finish(
    declaration: crate::ToolDeclaration,
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
    };
    let parked = Material::journal_local(
        MaterialOwner::Run {
            opener: owner.clone(),
        },
        MaterialRole::AttemptOutput,
        serde_json::to_string(&parked).expect("a parked call encodes"),
    )
    .parked("wait".to_owned());
    let terminal = crate::ProcessAwaitOutput::from_tool_output(ToolCallOutput::finish(
        crate::ToolValue::untrusted_json(value),
    ));
    let call = crate::sansio::PendingToolCall {
        call_id: crate::ToolCallId::fixture("parked-finish"),
        provider_call_id: None,
        tool_name: "submit".to_owned(),
        args: serde_json::json!({}),
        replay: None,
    };
    let resolution = Resolution::Ok(serde_json::to_value(terminal).expect("a terminal encodes"));
    let SettledOutput::Completed(material) = resolved_member(&owner, &call, &parked, resolution)
    else {
        panic!("a resolved park completes the call");
    };
    decode_completed(material.payload())
        .expect("the completed call decodes")
        .output
}

/// FIG-5823 law 1, deferred route: a Finish that arrives through a park's
/// resolution is checked against the value schema the park recorded. A
/// value it refuses fails the call with the typed mismatch and no control;
/// a value it admits settles carrying that schema as its witness.
#[test]
fn a_parked_finish_settles_under_the_declaration_its_park_recorded() {
    let declaration = crate::ToolDeclaration {
        controls: crate::TurnControls::finish(integer()),
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
                refusal: crate::DeclarationRefusal::FinishValueMismatch { value_schema, .. },
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
