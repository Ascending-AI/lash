use super::*;
use crate::ast::{Declaration, ProcessParam};
use crate::testing::ast_builders as builders;

#[derive(Debug, Deserialize, PartialEq)]
struct ScheduleSource {
    expr: String,
    #[serde(default)]
    tz: Option<String>,
}

fn resources() -> LashlangHostCatalog {
    let mut resources = LashlangHostCatalog::new();
    resources
        .add_trigger_source_constructor(
            ["cron", "Schedule"],
            TypeExpr::Object(vec![
                TypeField {
                    name: "expr".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
                TypeField {
                    name: "tz".into(),
                    ty: TypeExpr::Str,
                    optional: true,
                },
            ]),
            NamedDataType::object(
                "cron.Tick",
                vec![TypeField {
                    name: "fired_at".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid cron tick type"),
        )
        .expect("valid cron schedule source");
    resources
}

fn process_environment(resources: LashlangHostCatalog) -> crate::LashlangHostEnvironment {
    crate::LashlangHostEnvironment::new(resources, crate::LashlangAbilities::default())
}

/// Links `declarations` above `source = cron.Schedule({ expr: "*" })` and
/// `finish source`, the trailing main every trigger fixture shares.
fn linked_artifact(
    declarations: Vec<Declaration>,
    resources: LashlangHostCatalog,
) -> ModuleArtifact {
    let program = builders::module(
        declarations,
        vec![
            builders::assign(
                "source",
                builders::receiver_call(
                    builders::resource(&["cron"]),
                    "Schedule",
                    vec![builders::record(vec![("expr", builders::string("*"))])],
                ),
            ),
            builders::finish(builders::var("source")),
        ],
    );
    crate::LinkedModule::link(program, process_environment(resources))
        .expect("link trigger target module")
        .artifact
}

/// `process tick(<params>) { finish event.fired_at }`
fn tick_process(params: Vec<ProcessParam>) -> Declaration {
    builders::process(
        "tick",
        params,
        builders::block(vec![builders::finish(builders::field(
            builders::var("event"),
            "fired_at",
        ))]),
    )
}

fn event_input_template() -> TriggerInputTemplate {
    TriggerInputTemplate::new(BTreeMap::from([(
        "event".to_string(),
        TriggerInputBinding::Event,
    )]))
}

fn required_field(name: &str, ty: TypeExpr) -> TypeField {
    TypeField {
        name: name.into(),
        ty,
        optional: false,
    }
}

fn process_type(params: &[(&str, TypeExpr)], output: TypeExpr) -> TypeExpr {
    TypeExpr::Process(crate::ProcessType::known(
        crate::ProcessSignature::try_new(
            params
                .iter()
                .map(|(name, ty)| crate::ProcessParam {
                    name: (*name).into(),
                    ty: ty.clone(),
                })
                .collect(),
            output,
        )
        .unwrap(),
    ))
}

fn definition_for(artifact: &ModuleArtifact, process_name: &str) -> ProcessDefinitionIdentity {
    ProcessDefinitionIdentity::from_artifact_export(artifact, process_name)
        .expect("artifact should export process")
}

#[test]
fn resolved_type_assignability_accepts_any_as_source_or_target() {
    assert!(is_resolved_type_assignable(&TypeExpr::Any, &TypeExpr::Str));
    assert!(is_resolved_type_assignable(&TypeExpr::Str, &TypeExpr::Any));
}

#[test]
fn resolved_type_assignability_rejects_known_scalar_mismatches() {
    assert!(!is_resolved_type_assignable(&TypeExpr::Str, &TypeExpr::Int));
    assert!(!is_resolved_type_assignable(
        &TypeExpr::Bool,
        &TypeExpr::Float
    ));
}

#[test]
fn resolved_type_assignability_treats_strings_as_consistent_with_string_enums() {
    let target = TypeExpr::Enum(vec!["a".into(), "b".into()]);

    assert!(is_resolved_type_assignable(&TypeExpr::Str, &target));
    assert!(!is_resolved_type_assignable(&TypeExpr::Int, &target));
    assert!(is_resolved_type_assignable(
        &TypeExpr::Enum(vec!["a".into()]),
        &TypeExpr::Str
    ));
}

#[test]
fn resolved_type_assignability_checks_known_object_fields() {
    let target = TypeExpr::Object(vec![required_field("value", TypeExpr::Str)]);
    let compatible = TypeExpr::Object(vec![
        required_field("value", TypeExpr::Str),
        required_field("extra", TypeExpr::Int),
    ]);
    let incompatible = TypeExpr::Object(vec![required_field("value", TypeExpr::Int)]);

    assert!(is_resolved_type_assignable(&compatible, &target));
    assert!(!is_resolved_type_assignable(&incompatible, &target));
}

#[test]
fn resolved_type_assignability_treats_dict_as_gradual_at_object_targets() {
    let object = TypeExpr::Object(vec![required_field("value", TypeExpr::Str)]);

    assert!(is_resolved_type_assignable(&TypeExpr::Dict, &object));
    assert!(is_resolved_type_assignable(&object, &TypeExpr::Dict));
    assert!(!is_resolved_type_assignable(&TypeExpr::Int, &object));
}

/// `[]` is how a caller spells "no items", and `union_type` types it as
/// the empty-list sentinel `list[null]`. It has to reach a `list[T]`
/// parameter for every `T`, including through a union target such as
/// `list[dict] | null` (FIG-1421). A populated list keeps its real
/// element check.
#[test]
fn resolved_type_assignability_lets_an_empty_list_reach_any_list_target() {
    let empty = TypeExpr::List(Box::new(TypeExpr::Null));

    assert!(is_resolved_type_assignable(
        &empty,
        &TypeExpr::List(Box::new(TypeExpr::Dict))
    ));
    assert!(is_resolved_type_assignable(
        &empty,
        &TypeExpr::union(vec![
            TypeExpr::List(Box::new(TypeExpr::Dict)),
            TypeExpr::Null
        ])
    ));
    assert!(!is_resolved_type_assignable(
        &TypeExpr::List(Box::new(TypeExpr::Int)),
        &TypeExpr::List(Box::new(TypeExpr::Dict))
    ));
}

#[test]
fn resolved_type_assignability_rejects_named_nested_list_mismatches() {
    assert!(!is_resolved_type_assignable(
        &TypeExpr::List(Box::new(TypeExpr::Str)),
        &TypeExpr::List(Box::new(TypeExpr::Int)),
    ));
}

#[test]
fn resolved_type_assignability_accepts_named_nested_list_any_consistency() {
    assert!(is_resolved_type_assignable(
        &TypeExpr::List(Box::new(TypeExpr::Any)),
        &TypeExpr::List(Box::new(TypeExpr::Int)),
    ));
}

#[test]
fn process_assignability_preserves_names_order_variance_and_gradual_unknown() {
    let accepting_float = process_type(&[("value", TypeExpr::Float)], TypeExpr::Str);
    let accepting_int = process_type(&[("value", TypeExpr::Int)], TypeExpr::Str);
    assert!(is_resolved_type_assignable(
        &accepting_float,
        &accepting_int
    ));
    assert!(!is_resolved_type_assignable(
        &accepting_int,
        &accepting_float
    ));
    let accepting_any = process_type(&[("value", TypeExpr::Any)], TypeExpr::Str);
    assert!(is_resolved_type_assignable(&accepting_any, &accepting_int));
    assert!(is_resolved_type_assignable(&accepting_int, &accepting_any));

    let wide_output = process_type(&[("value", TypeExpr::Int)], TypeExpr::Any);
    assert!(is_resolved_type_assignable(&accepting_int, &wide_output));
    let returning_int = process_type(&[("value", TypeExpr::Int)], TypeExpr::Int);
    let returning_float = process_type(&[("value", TypeExpr::Int)], TypeExpr::Float);
    assert!(is_resolved_type_assignable(
        &returning_int,
        &returning_float
    ));
    assert!(!is_resolved_type_assignable(
        &returning_float,
        &returning_int
    ));

    let renamed = process_type(&[("payload", TypeExpr::Float)], TypeExpr::Str);
    assert!(!is_resolved_type_assignable(&accepting_float, &renamed));
    let reordered = process_type(
        &[("right", TypeExpr::Int), ("left", TypeExpr::Str)],
        TypeExpr::Str,
    );
    let ordered = process_type(
        &[("left", TypeExpr::Str), ("right", TypeExpr::Int)],
        TypeExpr::Str,
    );
    assert!(!is_resolved_type_assignable(&ordered, &reordered));

    let unknown = TypeExpr::Process(crate::ProcessType::unknown());
    assert!(is_resolved_type_assignable(&unknown, &ordered));
    assert!(is_resolved_type_assignable(&ordered, &unknown));
    assert!(!is_resolved_type_assignable(&unknown, &TypeExpr::Str));
}

#[test]
fn host_descriptor_encode_decode_and_typed_decode_round_trip() {
    let value = serde_json::json!({
        "expr": "*/10 * * * * *",
        "tz": "UTC",
    });
    let encoded = HostDescriptor::encode("cron.Schedule", value).expect("host descriptor encode");
    let decoded = HostDescriptor::decode(&encoded).expect("host descriptor decode");
    let payload: ScheduleSource = decoded
        .decode_as(&resources())
        .expect("typed host descriptor payload");

    assert_eq!(
        payload,
        ScheduleSource {
            expr: "*/10 * * * * *".to_string(),
            tz: Some("UTC".to_string()),
        }
    );
}

#[test]
fn host_descriptor_typed_decode_rejects_unknown_source_type() {
    let decoded = HostDescriptor::new("missing.Source", serde_json::json!({ "expr": "*" }));
    let err = decoded
        .decode_as::<ScheduleSource>(&resources())
        .expect_err("unknown source type should fail");

    assert!(
        matches!(err, HostDescriptorError::UnknownSourceType { source_type } if source_type == "missing.Source")
    );
}

#[test]
fn host_descriptor_typed_decode_reports_malformed_payload() {
    let decoded = HostDescriptor::new("cron.Schedule", serde_json::json!({ "expr": 1 }));
    let err = decoded
        .decode_as::<ScheduleSource>(&resources())
        .expect_err("malformed source payload should fail");

    assert!(
        matches!(err, HostDescriptorError::MalformedPayload { source_type, .. } if source_type == "cron.Schedule")
    );
}

#[test]
fn trigger_compatibility_accepts_matching_event_mapping() {
    let artifact = linked_artifact(
        vec![tick_process(vec![builders::param(
            "event",
            TypeExpr::Ref("cron.Tick".into()),
        )])],
        resources(),
    );
    let definition = definition_for(&artifact, "tick");

    let compatibility = check_trigger_compatibility(TriggerCompatibilityRequest {
        artifact: &artifact,
        definition: &definition,
        source_type: "cron.Schedule",
        inputs: &event_input_template(),
    })
    .expect("trigger should be compatible");

    assert_eq!(compatibility.definition, definition);
    assert_eq!(compatibility.event_type.name(), "cron.Tick");
    assert_eq!(
        format_type_expr(&compatibility.resolved_event_type),
        "{ fired_at: str }"
    );
}

#[test]
fn trigger_compatibility_rejects_unknown_source() {
    let artifact = linked_artifact(
        vec![tick_process(vec![builders::param(
            "event",
            TypeExpr::Ref("cron.Tick".into()),
        )])],
        resources(),
    );
    let definition = definition_for(&artifact, "tick");

    let err = check_trigger_compatibility(TriggerCompatibilityRequest {
        artifact: &artifact,
        definition: &definition,
        source_type: "missing.Source",
        inputs: &event_input_template(),
    })
    .expect_err("unknown source should fail");

    assert!(
        matches!(err, TriggerCompatibilityError::UnknownSourceType { source_type } if source_type == "missing.Source")
    );
}

#[test]
fn trigger_compatibility_rejects_wrong_process_ref() {
    let artifact = linked_artifact(
        vec![tick_process(vec![builders::param(
            "event",
            TypeExpr::Ref("cron.Tick".into()),
        )])],
        resources(),
    );
    let mut definition = definition_for(&artifact, "tick");
    definition.process_ref.pos = definition.process_ref.pos.saturating_add(1);

    let err = check_trigger_compatibility(TriggerCompatibilityRequest {
        artifact: &artifact,
        definition: &definition,
        source_type: "cron.Schedule",
        inputs: &event_input_template(),
    })
    .expect_err("wrong process ref should fail");

    assert!(matches!(
        err,
        TriggerCompatibilityError::ProcessRefMismatch { .. }
    ));
}

#[test]
fn trigger_compatibility_rejects_missing_event_input() {
    let artifact = linked_artifact(
        vec![tick_process(vec![builders::param(
            "event",
            TypeExpr::Ref("cron.Tick".into()),
        )])],
        resources(),
    );
    let definition = definition_for(&artifact, "tick");
    let inputs = TriggerInputTemplate::new(BTreeMap::from([(
        "event".to_string(),
        TriggerInputBinding::Fixed {
            value: serde_json::json!({"fired_at": "now"}),
        },
    )]));

    let err = check_trigger_compatibility(TriggerCompatibilityRequest {
        artifact: &artifact,
        definition: &definition,
        source_type: "cron.Schedule",
        inputs: &inputs,
    })
    .expect_err("missing event mapping should fail");

    assert!(matches!(
        err,
        TriggerCompatibilityError::MissingEventInput { .. }
    ));
}

#[test]
fn trigger_compatibility_rejects_unknown_input() {
    let artifact = linked_artifact(
        vec![tick_process(vec![builders::param(
            "event",
            TypeExpr::Ref("cron.Tick".into()),
        )])],
        resources(),
    );
    let definition = definition_for(&artifact, "tick");
    let inputs = TriggerInputTemplate::new(BTreeMap::from([
        ("event".to_string(), TriggerInputBinding::Event),
        ("extra".to_string(), TriggerInputBinding::Event),
    ]));

    let err = check_trigger_compatibility(TriggerCompatibilityRequest {
        artifact: &artifact,
        definition: &definition,
        source_type: "cron.Schedule",
        inputs: &inputs,
    })
    .expect_err("unknown input should fail");

    assert!(matches!(
        err,
        TriggerCompatibilityError::UnknownInput { input, .. }
            if input == "extra"
    ));
}

#[test]
fn trigger_compatibility_rejects_event_type_mismatch() {
    let artifact = linked_artifact(
        vec![tick_process(vec![builders::param(
            "event",
            TypeExpr::Object(vec![
                required_field("fired_at", TypeExpr::Str),
                required_field("required", TypeExpr::Str),
            ]),
        )])],
        resources(),
    );
    let definition = definition_for(&artifact, "tick");

    let err = check_trigger_compatibility(TriggerCompatibilityRequest {
        artifact: &artifact,
        definition: &definition,
        source_type: "cron.Schedule",
        inputs: &event_input_template(),
    })
    .expect_err("event mismatch should fail");

    assert!(matches!(
        err,
        TriggerCompatibilityError::EventMismatch { .. }
    ));
}

#[test]
fn trigger_compatibility_rejects_fixed_resource_mismatch() {
    let mut resources = resources();
    resources.ensure_resource_type("Bucket");
    let artifact = linked_artifact(
        vec![tick_process(vec![
            builders::param("event", TypeExpr::Ref("cron.Tick".into())),
            builders::param("bucket", TypeExpr::Ref("Bucket".into())),
        ])],
        resources,
    );
    let definition = definition_for(&artifact, "tick");
    let inputs = TriggerInputTemplate::new(BTreeMap::from([
        ("event".to_string(), TriggerInputBinding::Event),
        (
            "bucket".to_string(),
            TriggerInputBinding::Fixed {
                value: serde_json::json!({
                    "__resource__": true,
                    "type": "OtherBucket",
                    "alias": "wrong",
                }),
            },
        ),
    ]));

    let err = check_trigger_compatibility(TriggerCompatibilityRequest {
        artifact: &artifact,
        definition: &definition,
        source_type: "cron.Schedule",
        inputs: &inputs,
    })
    .expect_err("resource mismatch should fail");

    assert!(matches!(
        err,
        TriggerCompatibilityError::FixedInputMismatch { input, .. }
            if input == "bucket"
    ));
}
