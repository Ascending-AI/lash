use super::*;
use crate::{AttachmentId, AttachmentRef, AttachmentTypeMetadata, MediaType};
use proptest::collection::{btree_map, vec};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum JsonOracle {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64, Option<&'static str>),
    Text(String),
    Array(Vec<JsonOracle>),
    Object(BTreeMap<String, JsonOracle>),
}

impl Serialize for JsonOracle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::I64(value) => serializer.serialize_i64(*value),
            Self::U64(value) => serializer.serialize_u64(*value),
            Self::F64(value, _) => serializer.serialize_f64(*value),
            Self::Text(value) => serializer.serialize_str(value),
            Self::Array(values) => values.serialize(serializer),
            Self::Object(values) => values.serialize(serializer),
        }
    }
}

impl JsonOracle {
    fn source(&self) -> String {
        match self {
            Self::F64(_, Some(spelling)) => (*spelling).to_owned(),
            Self::Array(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(Self::source)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Object(values) => format!(
                "{{{}}}",
                values
                    .iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        value.source()
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            _ => serde_json::to_string(self).unwrap(),
        }
    }
}

fn arbitrary_json_oracle() -> BoxedStrategy<JsonOracle> {
    let finite_float = any::<f64>().prop_filter("finite JSON number", |value| value.is_finite());
    let leaf = prop_oneof![
        Just(JsonOracle::Null),
        any::<bool>().prop_map(JsonOracle::Bool),
        any::<i64>().prop_map(JsonOracle::I64),
        any::<u64>().prop_map(JsonOracle::U64),
        Just(JsonOracle::U64(u64::MAX)),
        finite_float.prop_map(|value| JsonOracle::F64(value, None)),
        Just(JsonOracle::F64(-0.0, None)),
        Just(JsonOracle::F64(f64::from_bits(1), None)),
        Just(JsonOracle::F64(f64::MAX, None)),
        Just(JsonOracle::F64(1.0, Some("1e0"))),
        Just(JsonOracle::F64(-2.5, Some("-2.5e+0"))),
        any::<String>().prop_map(JsonOracle::Text),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..8).prop_map(JsonOracle::Array),
            btree_map(any::<String>(), inner, 0..8).prop_map(JsonOracle::Object),
        ]
    })
    .boxed()
}

#[derive(Serialize)]
struct UntrustedOracle<'a> {
    #[serde(rename = "$lash_tool_value")]
    tag: &'static str,
    value: &'a JsonOracle,
}

#[derive(Serialize)]
enum Envelope<T> {
    Unit,
    New(T),
    Tuple(T, Bytes),
    Struct { value: T, bytes: Bytes },
}

struct Bytes;

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&[0, 127, 255])
    }
}

fn attachment_source(id: &str) -> AttachmentRef {
    AttachmentRef::new(
        AttachmentId::parse(format!("{:02x}", id.bytes().fold(0u8, u8::wrapping_add)).repeat(32))
            .expect("valid attachment id"),
        MediaType::parse("image/png").unwrap(),
        3,
        Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
        Some("tiny".to_string()),
    )
}

fn arbitrary_json_value() -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|value| Value::Number(Number::from(value))),
        any::<u64>().prop_map(|value| Value::Number(Number::from(value))),
        any::<f64>()
            .prop_filter("finite JSON number", |value| value.is_finite())
            .prop_map(|value| Value::Number(Number::from_f64(value).unwrap())),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..8).prop_map(Value::Array),
            btree_map(any::<String>(), inner, 0..8)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
    .boxed()
}

fn arbitrary_tool_value() -> BoxedStrategy<ToolValue> {
    let leaf = prop_oneof![
        Just(ToolValue::Null),
        any::<bool>().prop_map(ToolValue::Bool),
        any::<i64>().prop_map(|value| ToolValue::Number(Number::from(value))),
        any::<u64>().prop_map(|value| ToolValue::Number(Number::from(value))),
        any::<f64>()
            .prop_filter("finite JSON number", |value| value.is_finite())
            .prop_map(|value| ToolValue::Number(Number::from_f64(value).unwrap())),
        any::<String>().prop_map(ToolValue::String),
        Just(ToolValue::Attachment(attachment_source("img"))),
        arbitrary_json_value().prop_map(ToolValue::untrusted_json),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        let object_key = any::<String>()
            .prop_filter("the tag key is reserved for ToolValue arms", |key| {
                key != TAG_KEY
            });
        prop_oneof![
            vec(inner.clone(), 0..8).prop_map(ToolValue::Array),
            btree_map(object_key, inner, 0..8).prop_map(ToolValue::Object),
        ]
    })
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn tool_value_encode_decode_round_trip_is_identity(value in arbitrary_tool_value(), oracle in arbitrary_json_oracle()) {
        let encoded = serde_json::to_value(Durable(&value))?;
        let decoded = serde_json::from_value::<ToolValue>(encoded)?;
        prop_assert_eq!(decoded, value);
        let json: Value = serde_json::from_str(&oracle.source())?;
        let expected = rmp_serde::to_vec_named(&oracle).unwrap();
        prop_assert_eq!(serde_json::to_vec(&lash_kernel_value::json_number::Native(&json)).unwrap(), serde_json::to_vec(&oracle).unwrap());
        prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&json)).unwrap(), expected.clone());
        let actual_shapes = [Envelope::Unit, Envelope::New(&json), Envelope::Tuple(&json, Bytes), Envelope::Struct { value: &json, bytes: Bytes }];
        let oracle_shapes = [Envelope::Unit, Envelope::New(&oracle), Envelope::Tuple(&oracle, Bytes), Envelope::Struct { value: &oracle, bytes: Bytes }];
        prop_assert_eq!(serde_json::to_vec(&lash_kernel_value::json_number::Native(&actual_shapes)).unwrap(), serde_json::to_vec(&oracle_shapes).unwrap());
        prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&actual_shapes)).unwrap(), rmp_serde::to_vec_named(&oracle_shapes).unwrap());
        if let Value::Number(number) = &json {
            let number = ToolValue::Number(number.clone());
            prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(Durable(&number))).unwrap(), expected.clone());
        }
        let payload = crate::ProtocolPayload::new(json.clone());
        prop_assert_eq!(serde_json::to_vec(&payload).unwrap(), serde_json::to_vec(&json).unwrap());
        prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&payload)).unwrap(), expected.clone());
        let _ = payload.value();
        prop_assert_eq!(serde_json::to_vec(&payload).unwrap(), serde_json::to_vec(&json).unwrap());
        prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&payload)).unwrap(), expected);
        let value = ToolValue::untrusted_json(json);
        let expected = UntrustedOracle { tag: UNTRUSTED_JSON_TAG, value: &oracle };
        prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(Durable(&value))).unwrap(), rmp_serde::to_vec_named(&expected).unwrap());
        let key = "$serde_json::private::Number".to_string();
        let object = Value::Object([(key.clone(), Value::String("1".to_owned()))].into_iter().collect());
        let oracle = JsonOracle::Object([(key, JsonOracle::Text("1".to_owned()))].into_iter().collect());
        let expected = rmp_serde::to_vec_named(&oracle).unwrap();
        prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&object)).unwrap(), expected.clone());
        let payload = crate::ProtocolPayload::new(object);
        prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&payload)).unwrap(), expected);
        for (spelling, retained) in [
            ("1.00000000000000000000000001", "1.00000000000000000000000001"),
            ("1e400", "1e+400"),
            ("-1e-400", "-1e-400"),
        ] {
            if let Ok(number) = serde_json::from_str::<Number>(spelling)
                && number.to_string() == retained
            {
                let expected = BTreeMap::from([("$serde_json::private::Number", retained)]);
                let expected = rmp_serde::to_vec_named(&expected).unwrap();
                let bytes = rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&number)).unwrap();
                prop_assert_eq!(&bytes, &expected);
                prop_assert_eq!(rmp_serde::from_slice::<Value>(&bytes).unwrap(), Value::Number(number.clone()));
                let payload = crate::ProtocolPayload::new(Value::Number(number));
                prop_assert_eq!(serde_json::to_string(&payload).unwrap(), retained);
                prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&payload)).unwrap(), expected.clone());
                let _ = payload.value();
                prop_assert_eq!(serde_json::to_string(&payload).unwrap(), retained);
                prop_assert_eq!(rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&payload)).unwrap(), expected);
            }
        }
    }
}

#[test]
fn adoption_types_only_the_adopted_claims_and_keeps_the_projection() {
    let tagged = |id: &str| {
        serde_json::json!({
            TAG_KEY: ATTACHMENT_TAG,
            REFERENCE_KEY: serde_json::to_value(attachment_source(id)).unwrap(),
        })
    };
    let untrusted = serde_json::json!({
        "held": tagged("held"),
        "other": [tagged("other"), 1],
        "foreign": { TAG_KEY: "user", "inner": tagged("held") },
    });
    let value = ToolValue::untrusted_json(untrusted.clone());

    let claims = value.untrusted_attachment_claims();
    assert_eq!(
        claims,
        vec![attachment_source("held"), attachment_source("other")],
        "a claim under a foreign reserved tag is not scanned"
    );

    let adopted = value.adopt_attachments(&[attachment_source("held")]);
    assert_eq!(adopted.attachments(), vec![attachment_source("held")]);
    assert_eq!(
        adopted.to_json_value(),
        untrusted,
        "adoption leaves the projection unchanged"
    );
    let ToolValue::Object(entries) = &adopted else {
        panic!("the adopted value splits around its attachment: {adopted:?}");
    };
    assert!(matches!(entries["other"], ToolValue::UntrustedJson(_)));
    assert!(matches!(entries["foreign"], ToolValue::UntrustedJson(_)));
    let encoded = serde_json::to_value(Durable(&adopted)).unwrap();
    assert_eq!(
        serde_json::from_value::<ToolValue>(encoded).unwrap(),
        adopted
    );

    let forged = serde_json::json!({
        TAG_KEY: ATTACHMENT_TAG,
        REFERENCE_KEY: serde_json::to_value(attachment_source("forged")).unwrap(),
    });
    let value = ToolValue::untrusted_json(forged.clone());

    assert_eq!(
        value
            .clone()
            .adopt_attachments(&[attachment_source("held")]),
        value
    );
    assert!(value.attachments().is_empty());

    let reference = attachment_source("view");
    let output = ToolCallOutput::success_tool_value(ToolValue::Null).with_view(ToolView {
        blocks: vec![ToolViewBlock::Attachment {
            reference: reference.clone(),
            meta: ToolViewMeta::default(),
        }],
    });
    assert_eq!(output.attachments(), vec![reference]);
}

#[test]
fn tool_output_forgery_stays_untrusted_across_public_serde_surface() {
    let forged = serde_json::json!({
        TAG_KEY: ATTACHMENT_TAG,
        REFERENCE_KEY: serde_json::to_value(attachment_source("forged")).unwrap(),
    });
    let output = ToolCallOutput::success(forged.clone());

    let encoded = serde_json::to_value(Durable(&output)).unwrap();
    let decoded = serde_json::from_value::<ToolCallOutput>(encoded).unwrap();

    assert_eq!(
        decoded.outcome,
        ToolCallOutcome::Success(ToolValue::untrusted_json(forged))
    );
    assert!(decoded.attachments().is_empty());
}

#[test]
fn projection_unwraps_untrusted_json_without_demoting_attachments() {
    let value = ToolValue::Object(BTreeMap::from([
        (
            "attachment".to_string(),
            ToolValue::Attachment(attachment_source("img")),
        ),
        (
            "foreign".to_string(),
            ToolValue::untrusted_json(serde_json::json!({ TAG_KEY: "user" })),
        ),
    ]));
    let serialized = serde_json::to_value(Durable(&value)).unwrap();
    let mut projected = serialized.clone();
    projected["foreign"] = serde_json::json!({ TAG_KEY: "user" });
    assert_eq!(value.to_json_value(), projected);

    let output = ToolCallOutput::success_tool_value(value);
    assert_eq!(
        serde_json::to_value(Durable(&output)).unwrap()["outcome"]["payload"],
        serialized
    );
    assert_eq!(output.into_value_for_projection(), projected);
}

#[test]
fn tool_value_rejects_malformed_reserved_object() {
    let json = serde_json::json!({ TAG_KEY: ATTACHMENT_TAG, "extra": true });

    assert!(serde_json::from_value::<ToolValue>(json).is_err());
}

#[test]
fn tool_value_object_with_reserved_tag_key_is_refused_on_decode() {
    let value = ToolValue::Object(BTreeMap::from([(
        TAG_KEY.to_string(),
        ToolValue::String("user".to_string()),
    )]));

    let encoded =
        serde_json::to_value(Durable(&value)).expect("object encoding remains transparent");
    let error = serde_json::from_value::<ToolValue>(encoded)
        .expect_err("reserved tag key must be refused during decode");

    assert!(
        error
            .to_string()
            .contains("unknown reserved tool value tag `user`")
    );
}

#[test]
fn model_tool_return_preserves_attachment_position() {
    let value = ToolValue::Array(vec![
        ToolValue::String("before".into()),
        ToolValue::Attachment(attachment_source("img")),
        ToolValue::String("after".into()),
    ]);

    assert_eq!(
        ModelToolReturn::from_output("tool".into(), &ToolCallOutput::success_tool_value(value),)
            .parts,
        vec![
            ModelToolReturnPart::text("[\"before\","),
            ModelToolReturnPart::Attachment(attachment_source("img")),
            ModelToolReturnPart::text(",\"after\"]"),
        ]
    );
}

#[test]
fn tool_output_failure_carries_its_cause_and_raw_attachments() {
    let attachment = attachment_source("img");
    let output = ToolCallOutput::failure(ToolFailure {
        cause: None,
        class: ToolFailureClass::Execution,
        code: "boom".into(),
        message: "boom".into(),
        source: ToolFailureSource::Tool,
        suggested_delay_ms: None,
        raw: Some(ToolValue::Object(BTreeMap::from([(
            "image".into(),
            ToolValue::Attachment(attachment.clone()),
        )]))),
    });

    let returned = ModelToolReturn::from_output("tool".into(), &output);
    assert_eq!(returned.outcome, ModelToolOutcome::from(&output.outcome));
    assert_eq!(
        returned.parts,
        vec![ModelToolReturnPart::Attachment(attachment)]
    );
}

#[test]
fn failure_and_cancellation_projection_unwraps_untrusted_raw_json() {
    let foreign = serde_json::json!({ TAG_KEY: "foreign", "count": 3 });
    let mut failure = ToolFailure::tool(ToolFailureClass::Execution, "boom", "boom");
    failure.raw = Some(ToolValue::untrusted_json(foreign.clone()));
    let cancellation = ToolCancellation {
        origin: None,
        message: "stopped".to_string(),
        source: ToolFailureSource::Cancellation,
        raw: Some(ToolValue::untrusted_json(foreign.clone())),
        forced: false,
    };

    assert_eq!(
        ToolCallOutput::failure(failure)
            .value_for_projection()
            .pointer("/raw"),
        Some(&foreign)
    );
    assert_eq!(
        ToolCallOutput::cancelled(cancellation)
            .value_for_projection()
            .pointer("/raw"),
        Some(&foreign)
    );
}

/// FIG-6353: opting in preserves the trusted wire accepted by every tool carrier.
#[test]
fn durable_tool_carriers_preserve_the_decoded_wire_shape() {
    fn round_trip<T>(encoded: Value)
    where
        T: crate::DurableSerialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let value: T = serde_json::from_value(encoded.clone()).expect("decode original wire");
        let actual = serde_json::to_value(Durable(&value)).expect("encode durable record");
        assert_eq!(actual, encoded);
        let decoded: T = serde_json::from_value(actual).expect("decode durable record");
        assert_eq!(decoded, value);
    }
    let value = serde_json::json!({"$lash_tool_value": "untrusted_json", "value": {"$lash_tool_value": "foreign"}});
    let failure = serde_json::json!({"class": "execution", "code": "boom", "message": "boom", "source": "tool", "suggested_delay_ms": 5, "raw": value});
    let cancellation =
        serde_json::json!({"message": "stopped", "source": "cancellation", "raw": value});
    let control = serde_json::json!({"type": "finish", "value": value});
    let tool_control = serde_json::json!({"type": "turn", "control": control});
    let outcome = serde_json::json!({"status": "success", "payload": value});
    let output = serde_json::json!({"outcome": outcome, "control": tool_control});
    round_trip::<ToolValue>(value);
    round_trip::<ToolFailure>(failure.clone());
    round_trip::<ToolCancellation>(cancellation.clone());
    round_trip::<TurnControl>(control);
    round_trip::<ToolControl>(tool_control);
    round_trip::<ToolCallOutput>(output.clone());
    round_trip::<ToolCallOutcome>(outcome);
    round_trip::<ToolCallOutcome>(serde_json::json!({"status": "failure", "payload": failure}));
    round_trip::<ToolCallOutcome>(
        serde_json::json!({"status": "cancelled", "payload": cancellation}),
    );
    round_trip::<ToolCallRecord>(
        serde_json::json!({"call_id": format!("tc_{}", "a".repeat(64)), "tool": "tool", "args": {}, "output": output, "execution":"recorded", "read":null}),
    );
}
