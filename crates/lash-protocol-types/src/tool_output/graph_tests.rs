#![allow(clippy::expect_used, clippy::unwrap_used)]

use lash_kernel_doc::graph_tokens::{decode_body, encode_body};

use super::*;
use crate::{AttachmentId, AttachmentRef, MediaType, ToolCallOutput, ToolFailure};

fn attachment() -> AttachmentRef {
    AttachmentRef::new(
        AttachmentId::parse("ab".repeat(32)).unwrap(),
        MediaType::parse("image/png").unwrap(),
        12,
        None,
        Some("plot".to_owned()),
    )
}

/// PC-OUTCOME (presentation half, FIG-5887 §4): a recipe rebuilds exactly
/// the presentation it recorded, from the typed outcome alone, storing the
/// rendered text once rather than as a copy; and it refuses a different
/// outcome or another renderer instead of re-rendering.
#[test]
fn a_recipe_rebuilds_exactly_the_recorded_presentation_or_refuses() {
    let long = "z".repeat(4_000);
    let success = ToolCallOutput::success(serde_json::json!({"rows": [1, 2.5, -3], "text": long}));
    let rendered = ModelToolReturn::from_output("query".to_owned(), &success);
    let rendered_text = rendered.parts[0].visible_text().unwrap().to_owned();

    // Retention: the witness is the rendering's first bytes and a notice.
    let mut retained = rendered.clone();
    retained.parts = vec![ModelToolReturnPart::Retained(crate::RetainedOutput {
        reference: attachment(),
        witness: format!("{}\n[output retained]", &rendered_text[..100]),
    })];
    // A plugin step appended a note and an attachment to a failure.
    let failure = ToolCallOutput::failure(ToolFailure::io("disk", "the disk is full"));
    let mut appended = ModelToolReturn::from_output("write".to_owned(), &failure);
    appended
        .parts
        .push(ModelToolReturnPart::Attachment(attachment()));
    appended
        .parts
        .push(ModelToolReturnPart::text("free some space"));

    for (presentation, output, expect_parts) in [
        (&rendered, &success, vec![RecipePart::Rendered]),
        (
            &retained,
            &success,
            vec![RecipePart::Retained {
                reference: attachment(),
                witness: vec![
                    RecipeText::Slice { start: 0, len: 100 },
                    RecipeText::Literal("\n[output retained]".to_owned()),
                ],
            }],
        ),
        (
            &appended,
            &failure,
            vec![
                RecipePart::Attachment(attachment()),
                RecipePart::Text(vec![RecipeText::Literal("free some space".to_owned())]),
            ],
        ),
    ] {
        let recipe = PresentationRecipe::of(presentation, output);
        assert_eq!(recipe.parts, expect_parts);
        let stored: PresentationRecipe = decode_body(&encode_body(&recipe)).unwrap();
        assert_eq!(stored.rebuild(output).unwrap(), *presentation);
        // The recipe never holds the rendered text itself.
        assert!(encode_body(&recipe).len() < 1_000);

        let other = ToolCallOutput::success(serde_json::json!("other"));
        assert_eq!(stored.rebuild(&other), Err(PresentationRefusal::Mismatch));
        let mut foreign = stored.clone();
        foreign.renderer = RendererId([7; 32]);
        assert_eq!(
            foreign.rebuild(output),
            Err(PresentationRefusal::RendererUnavailable)
        );
    }
}

/// A tool output has one canonical encoding: it round-trips exactly, and a
/// JSON object whose keys are out of order, or a non-negative integer
/// written as signed, is refused.
#[test]
fn a_tool_output_has_one_canonical_encoding() {
    let output = ToolCallOutput::success_tool_value(ToolValue::Object(
        [
            ("a".to_owned(), ToolValue::Attachment(attachment())),
            (
                "b".to_owned(),
                ToolValue::untrusted_json(serde_json::json!({"x": [null, true, 1, -1, 0.5]})),
            ),
        ]
        .into_iter()
        .collect(),
    ));
    let bytes = encode_body(&output);
    assert_eq!(decode_body::<ToolCallOutput>(&bytes).unwrap(), output);
    assert_eq!(
        encode_body(&decode_body::<ToolCallOutput>(&bytes).unwrap()),
        bytes
    );

    let mut unsorted = TokenWriter::new();
    unsorted.sum(5, 1);
    unsorted.map(2);
    unsorted.text("b");
    unsorted.sum(0, 0);
    unsorted.text("a");
    unsorted.sum(0, 0);
    let bytes = unsorted.into_bytes();
    assert!(decode_json(&mut TokenReader::new(&bytes)).is_err());

    let mut signed = TokenWriter::new();
    signed.sum(2, 1);
    signed.sum(1, 1);
    signed.signed(3);
    let bytes = signed.into_bytes();
    assert!(decode_json(&mut TokenReader::new(&bytes)).is_err());
}

/// FIG-6518: a stored derivation retains the failure independently of the presented words.
#[test]
fn a_stored_failure_derivation_retains_the_typed_cause() {
    let output = ToolCallOutput::failure(ToolFailure::io("disk", "boom"));
    let mut presented = ModelToolReturn::from_output("write".into(), &output);
    presented.parts = vec![ModelToolReturnPart::text("boom")];
    let recipe = PresentationRecipe::of(&presented, &output);
    assert_eq!(
        recipe.parts,
        vec![RecipePart::Text(vec![RecipeText::Slice {
            start: 0,
            len: 4
        }])]
    );
    let stored: PresentationRecipe = decode_body(&encode_body(&recipe)).unwrap();
    let rebuilt = stored.rebuild(&output).unwrap();
    let encoded = serde_json::to_value(&rebuilt).unwrap();
    assert_eq!(encoded["outcome"]["status"], "failure");
    assert_eq!(encoded["outcome"]["payload"]["code"], "disk");
    assert_eq!(encoded["outcome"]["payload"]["message"], "boom");
    assert_eq!(rebuilt.parts, presented.parts);
    let different = ToolCallOutput::failure(ToolFailure::io("other", "boom"));
    assert_eq!(
        stored.rebuild(&different),
        Err(PresentationRefusal::Mismatch)
    );
}
