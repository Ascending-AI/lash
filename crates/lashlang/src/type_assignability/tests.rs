use super::*;

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

/// A process value evaluates to its immutable definition record, so it fills a
/// contract slot that names that record (`processes.start`'s `definition`),
/// and no other record shape.
#[test]
fn resolved_type_assignability_lets_a_process_fill_its_definition_record() {
    let id = json!({"type": "object", "properties": {"$lash_definition_id": {"type": "string", "pattern": "^lash\\.definition:sha256:[0-9a-f]{64}$"}}, "required": ["$lash_definition_id"], "additionalProperties": false});
    let definition = json_schema_to_type_expr(&json!({"type": "object", "properties": {"id": id, "signature": {"anyOf": [{"type": "object", "properties": {"signature": {"const": "unknown"}}, "required": ["signature"], "additionalProperties": false}, {"type": "object", "properties": {"signature": {"const": "known"}, "encoding": {}}, "required": ["signature", "encoding"], "additionalProperties": false}]}}, "required": ["id", "signature"], "additionalProperties": false}))
    .expect("the definition record schema imports");
    let process = process_type(&[], TypeExpr::Str);

    assert!(is_resolved_type_assignable(&process, &definition));
    assert!(is_resolved_type_assignable(
        &TypeExpr::Process(crate::ProcessType::unknown()),
        &definition
    ));
    assert!(!is_resolved_type_assignable(
        &process,
        &TypeExpr::Object(vec![
            required_field("id", TypeExpr::Str),
            required_field("signature", TypeExpr::Any),
        ])
    ));
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
